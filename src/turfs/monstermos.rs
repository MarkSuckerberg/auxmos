use super::*;

use std::collections::{HashMap, BTreeSet};

use indexmap::IndexSet;

use auxcallback::byond_callback_sender;

use std::cell::Cell;

type TransferInfo = [f32; 7];

type MixWithID = (TurfID, TurfMixture);

#[derive(Copy, Clone, Default)]
struct MonstermosInfo {
	transfer_dirs: TransferInfo,
	mole_delta: f32,
	curr_transfer_amount: f32,
	curr_transfer_dir: usize,
	last_slow_queue_cycle: i32,
	fast_done: bool,
}

const OPP_DIR_INDEX: [usize; 7] = [1, 0, 3, 2, 5, 4, 6];

//only used by slow decomp
const _DECOMP_REMOVE_RATIO: f32 = 5.0;

impl MonstermosInfo {
	fn adjust_eq_movement(&mut self, adjacent: &mut Self, dir_index: usize, amount: f32) {
		self.transfer_dirs[dir_index] += amount;
		if dir_index != 6 {
			adjacent.transfer_dirs[OPP_DIR_INDEX[dir_index]] -= amount;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_eq_movement() {
		let mut info_a: MonstermosInfo = Default::default();
		let mut info_b: MonstermosInfo = Default::default();
		info_a.adjust_eq_movement(&mut info_b, 1, 5.0);
		assert_eq!(info_a.transfer_dirs[1], 5.0);
		assert_eq!(info_b.transfer_dirs[0], -5.0);
	}
}

fn finalize_eq(
	i: TurfID,
	turf: &TurfMixture,
	info: &HashMap<TurfID, Cell<MonstermosInfo>>,
	max_x: i32,
	max_y: i32,
) {
	let sender = byond_callback_sender();
	let transfer_dirs = {
		let monstermos_orig = info.get(&i).unwrap();
		let mut monstermos_copy = monstermos_orig.get();
		let transfer_dirs = monstermos_copy.transfer_dirs;
		monstermos_copy
			.transfer_dirs
			.iter_mut()
			.for_each(|a| *a = 0.0); // null it out to prevent infinite recursion.
		monstermos_orig.set(monstermos_copy);
		transfer_dirs
	};
	let planet_transfer_amount = transfer_dirs[6];
	if planet_transfer_amount > 0.0 {
		if turf.total_moles() < planet_transfer_amount {
			finalize_eq_neighbors(i, turf, transfer_dirs, info, max_x, max_y);
		}
		GasArena::with_all_mixtures(|all_mixtures| {
			all_mixtures
				.get(turf.mix)
				.unwrap()
				.write()
				.remove(planet_transfer_amount);
		})
	} else if planet_transfer_amount < 0.0 {
		if let Some(air_entry) = turf.planetary_atmos.and_then(|i| planetary_atmos().get(&i)) {
			let planet_air = air_entry.value();
			let planet_sum = planet_air.total_moles();
			if planet_sum > 0.0 {
				GasArena::with_all_mixtures(|all_mixtures| {
					all_mixtures
						.get(turf.mix)
						.unwrap()
						.write()
						.merge(&(planet_air * (-planet_transfer_amount / planet_sum)));
				});
			}
		}
	}
	for (j, adj_id) in adjacent_tile_ids(turf.adjacency, i, max_x, max_y) {
		let amount = transfer_dirs[j as usize];
		if amount > 0.0 {
			if turf.total_moles() < amount {
				finalize_eq_neighbors(i, turf, transfer_dirs, info, max_x, max_y);
			}
			if let Some(adj_orig) = info.get(&adj_id) {
				if let Some(adj_turf) = turf_gases().get(&adj_id) {
					let mut adj_info = adj_orig.get();
					adj_info.transfer_dirs[OPP_DIR_INDEX[j as usize]] = 0.0;
					if turf.mix != adj_turf.mix {
						GasArena::with_all_mixtures(|all_mixtures| {
							let our_entry = all_mixtures.get(turf.mix).unwrap();
							let their_entry = all_mixtures.get(adj_turf.mix).unwrap();
							let mut air = our_entry.write();
							let mut other_air = their_entry.write();
							other_air.merge(&air.remove(amount));
						});
					}
					adj_orig.set(adj_info);
					sender
						.send(Box::new(move || {
							let real_amount = Value::from(-amount);
							let turf = unsafe { Value::turf_by_id_unchecked(i as u32) };
							let other_turf = unsafe { Value::turf_by_id_unchecked(adj_id as u32) };
							if let Err(e) = turf
								.call("consider_pressure_difference", &[&other_turf, &real_amount])
							{
								turf.call(
									"stack_trace",
									&[&Value::from_string(e.message.as_str())?],
								)
								.unwrap();
							}
							Ok(Value::null())
						}))
						.unwrap();
				}
			}
		}
	}
}

fn finalize_eq_neighbors(
	i: TurfID,
	turf: &TurfMixture,
	transfer_dirs: [f32; 7],
	info: &HashMap<TurfID, Cell<MonstermosInfo>>,
	max_x: i32,
	max_y: i32,
) {
	for (j, adjacent_id) in adjacent_tile_ids(turf.adjacency, i, max_x, max_y) {
		let amount = transfer_dirs[j as usize];
		if amount < 0.0 {
			if let Some(other_turf) = turf_gases().get(&adjacent_id) {
				finalize_eq(adjacent_id, other_turf.value(), info, max_x, max_y);
			}
		}
	}
}

#[cfg(feature = "explosive_decompression")]
fn explosively_depressurize(
	turf_idx: TurfID,
	turf: TurfMixture,
	mut info: HashMap<TurfID, Cell<MonstermosInfo>>,
	equalize_hard_turf_limit: usize,
	max_x: i32,
	max_y: i32,
) -> DMResult {
	let mut turfs: IndexSet<MixWithID> = IndexSet::new();
	let mut progression_order: IndexSet<MixWithID> = IndexSet::new();
	turfs.insert((turf_idx, turf));
	let cur_orig = info.entry(turf_idx).or_default();
	let mut cur_info: MonstermosInfo = Default::default();
	cur_info.curr_transfer_dir = 6;
	cur_orig.set(cur_info);
	let mut warned_about_planet_atmos = false;
	let mut cur_queue_idx = 0;
	while cur_queue_idx < turfs.len() {
		let (i, m) = turfs[cur_queue_idx];
		cur_queue_idx += 1;
		let cur_orig = info.entry(i).or_default();
		let mut cur_info = cur_orig.get();
		cur_info.curr_transfer_dir = 6;
		cur_orig.set(cur_info);
		if m.planetary_atmos.is_some() {
			warned_about_planet_atmos = true;
			continue;
		}
		if m.is_immutable() {
			if progression_order.insert((i, m)) {
				unsafe { Value::turf_by_id_unchecked(i) }
					.set(byond_string!("pressure_specific_target"), &unsafe {
						Value::turf_by_id_unchecked(i)
					})?;
			}
		} else {
			if cur_queue_idx > equalize_hard_turf_limit {
				continue;
			}
			for (_, loc) in adjacent_tile_ids(m.adjacency, i, max_x, max_y) {
				let adj_m = {
					*turf_gases().get(&loc).unwrap()
				};
				if turfs.insert((loc, adj_m)) {
					unsafe { Value::turf_by_id_unchecked(i) }.call(
						"consider_firelocks",
						&[&unsafe { Value::turf_by_id_unchecked(loc) }],
					)?;
					info.entry(loc).or_default().take();
				}
			}
		}
		if warned_about_planet_atmos {
			return Ok(Value::null()); // planet atmos > space
		}
	}
	for (i, _) in progression_order.iter() {
		let cur_info = info.entry(*i).or_default().get_mut();
		cur_info.curr_transfer_dir = 6;
	}
	let spess_turfs_len = progression_order.len();
	let mut total_moles: f64 = 0.0;
	cur_queue_idx = 0;
	while cur_queue_idx < progression_order.len() {
		let (i, m) = progression_order[cur_queue_idx];
		cur_queue_idx += 1;
		if cur_queue_idx > equalize_hard_turf_limit {
			continue;
		}
		for (j, loc) in adjacent_tile_ids(m.adjacency, i, max_x, max_y) {
			let adj_m = {
				*turf_gases().get(&loc).unwrap()
			};
			let adj_orig = info.entry(loc).or_default();
			let mut adj_info = adj_orig.get();
			if !adj_m.is_immutable() {
				if progression_order.insert((loc, adj_m)) {
					adj_info.curr_transfer_dir = OPP_DIR_INDEX[j as usize];
					adj_info.curr_transfer_amount = 0.0;
					let cur_target_turf = unsafe { Value::turf_by_id_unchecked(i) }
						.get(byond_string!("pressure_specific_target"))?;
					unsafe { Value::turf_by_id_unchecked(loc) }
						.set(byond_string!("pressure_specific_target"), &cur_target_turf)?;
					adj_orig.set(adj_info);
					total_moles += adj_m.total_moles() as f64;
				}
			}
		}
	}
	let _moles_sucked = (total_moles
		/ ((progression_order.len() - spess_turfs_len) as f64)) as f32
		/ _DECOMP_REMOVE_RATIO;
	let hpd = auxtools::Value::globals()
		.get(byond_string!("SSair"))?
		.get_list(byond_string!("high_pressure_delta"))
		.map_err(|_| {
			runtime!(
				"Attempt to interpret non-list value as list {} {}:{}",
				std::file!(),
				std::line!(),
				std::column!()
			)
		})?;
	for (i, m) in progression_order.iter().rev() {
		let cur_orig = info.entry(*i).or_default();
		let mut cur_info = cur_orig.get();
		if cur_info.curr_transfer_dir == 6 {
			continue;
		}
		let mut in_hpd = false;
		for k in 1..=hpd.len() {
			if hpd.get(k).unwrap() == unsafe { Value::turf_by_id_unchecked(*i) } {
				in_hpd = true;
				break;
			}
		}
		if !in_hpd {
			hpd.append(&unsafe { Value::turf_by_id_unchecked(*i) });
		}
		let loc = adjacent_tile_id(cur_info.curr_transfer_dir as u8, *i, max_x, max_y);
		let adj_m = {
			*turf_gases().get(&loc).unwrap()
		};
		let sum = adj_m.total_moles();
		cur_info.curr_transfer_amount += sum;
		cur_orig.set(cur_info);

		let adj_orig = info.entry(loc).or_default();
		let mut adj_info = adj_orig.get();

		adj_info.curr_transfer_amount += cur_info.curr_transfer_amount;
		adj_orig.set(adj_info);

		let byond_turf = unsafe { Value::turf_by_id_unchecked(*i) };

		byond_turf.set(
			byond_string!("pressure_difference"),
			Value::from(cur_info.curr_transfer_amount),
		)?;
		byond_turf.set(
			byond_string!("pressure_direction"),
			Value::from((1 << cur_info.curr_transfer_dir) as f32),
		)?;

		if adj_info.curr_transfer_dir == 6 {
			let byond_turf_adj = unsafe { Value::turf_by_id_unchecked(loc) };
			byond_turf_adj.set(
				byond_string!("pressure_difference"),
				Value::from(adj_info.curr_transfer_amount),
			)?;
			byond_turf_adj.set(
				byond_string!("pressure_direction"),
				Value::from((1 << cur_info.curr_transfer_dir) as f32),
			)?;
		}

		#[cfg(not(feature = "slow_decompression"))]
		{
			m.clear_air();
		}
		#[cfg(feature = "slow_decompression")]
		{
			m.clear_vol(_moles_sucked);
		}

		byond_turf.call("handle_decompression_floor_rip", &[&Value::from(sum)])?;
	}
	Ok(Value::null())
	//	if (total_gases_deleted / turfs.len() as f32) > 20.0 && turfs.len() > 10 { // logging I guess
	//	}
}

fn flood_fill_equalize_turfs(
	i: TurfID,
	m: TurfMixture,
	equalize_turf_limit: usize,
	equalize_hard_turf_limit: usize,
	max_x: i32,
	max_y: i32,
	found_turfs: &mut BTreeSet<TurfID>,
	info: &mut HashMap<TurfID, Cell<MonstermosInfo>>,
) -> Option<(IndexSet<MixWithID>, IndexSet<MixWithID>, f64)> {
	let mut turfs: IndexSet<MixWithID> = IndexSet::with_capacity(equalize_hard_turf_limit);
	let mut border_turfs: IndexSet<MixWithID> = IndexSet::with_capacity(equalize_turf_limit);
	let mut planet_turfs: IndexSet<MixWithID> = IndexSet::new();
	#[cfg(feature = "explosive_decompression")]
	let sender = byond_callback_sender();
	let mut total_moles = 0.0_f64;
	border_turfs.insert((i, m));
	found_turfs.insert(i);
	#[allow(unused_mut)]
	let mut space_this_time = false;
	loop {
		if turfs.len() >= equalize_hard_turf_limit {
			break;
		}
		if let Some((cur_idx, cur_turf)) = border_turfs.shift_remove_index(0 as usize) {
			if turfs.len() < equalize_turf_limit {
				if cur_turf.planetary_atmos.is_some() {
					planet_turfs.insert((cur_idx, cur_turf));
					continue;
				}
				total_moles += cur_turf.total_moles() as f64;
			}
			for (_, loc) in adjacent_tile_ids(cur_turf.adjacency, cur_idx, max_x, max_y) {
				if found_turfs.insert(loc) {
					if let Some(adj_turf) = turf_gases().get(&loc) {
						let adj_orig = info.entry(loc).or_default();
						#[cfg(feature = "explosive_decompression")]
						{
							adj_orig.take();
							border_turfs.insert((loc, *adj_turf.value()));
							if adj_turf.value().is_immutable() {
								// Uh oh! looks like someone opened an airlock to space! TIME TO SUCK ALL THE AIR OUT!!!
								// NOT ONE OF YOU IS GONNA SURVIVE THIS
								// (I just made explosions less laggy, you're welcome)
								turfs.insert((loc, *adj_turf.value()));
								let fake_cloned = info
									.iter()
									.map(|(&k, v)| (k, v.get()))
									.collect::<HashMap<TurfID, MonstermosInfo>>();
								let _ = sender.send(Box::new(move || {
									let cloned = fake_cloned
										.iter()
										.map(|(&k, &v)| (k, Cell::new(v)))
										.collect::<HashMap<TurfID, Cell<MonstermosInfo>>>();
									explosively_depressurize(
										i,
										m,
										cloned,
										equalize_hard_turf_limit,
										max_x,
										max_y,
									)
								}));
								space_this_time = true;
							}
						}
						#[cfg(not(feature = "explosive_decompression"))]
						{
							if adj_turf.enabled() {
								adj_orig.take();
								border_turfs.insert((loc, *adj_turf.value()));
							}
						}
					}
				}
				if space_this_time {
					break;
				}
			}
			turfs.insert((cur_idx, cur_turf));
		} else {
			break;
		}
	}
	(!space_this_time).then(|| (turfs, planet_turfs, total_moles))
}

fn monstermos_fast_process(
	i: TurfID,
	m: TurfMixture,
	max_x: i32,
	max_y: i32,
	info: &mut HashMap<TurfID, Cell<MonstermosInfo>>,
) {
	let cur_orig = info.get(&i).unwrap();
	let mut cur_info = cur_orig.get();
	cur_info.fast_done = true;
	let mut eligible_adjacents: i32 = 0;
	if cur_info.mole_delta > 0.0 {
		for (j, loc) in adjacent_tile_ids(m.adjacency, i, max_x, max_y) {
			if let Some(adj_orig) = info.get(&loc) {
				let adj_info = adj_orig.get();
				if !adj_info.fast_done {
					eligible_adjacents |= 1 << j;
				}
			}
		}
		let amt_eligible = eligible_adjacents.count_ones();
		if amt_eligible == 0 {
			cur_orig.set(cur_info);
			return;
		}
		let moles_to_move = cur_info.mole_delta / amt_eligible as f32;
		for (j, loc) in adjacent_tile_ids(eligible_adjacents as u8, i, max_x, max_y) {
			let adj_orig = info.get(&loc).unwrap();
			let mut adj_info = adj_orig.get();
			cur_info.adjust_eq_movement(&mut adj_info, j as usize, moles_to_move);
			cur_info.mole_delta -= moles_to_move;
			adj_info.mole_delta += moles_to_move;
			cur_orig.set(cur_info);
			adj_orig.set(adj_info);
		}
	}
	cur_orig.set(cur_info);
}

fn give_to_takers(
	giver_turfs: &Vec<MixWithID>,
	taker_turfs: &Vec<MixWithID>,
	max_x: i32,
	max_y: i32,
	info: &HashMap<TurfID, Cell<MonstermosInfo>>,
	queue_cycle_slow: &mut i32,
) {
	let mut queue: IndexSet<MixWithID> = IndexSet::with_capacity(taker_turfs.len());
	for (i, m) in giver_turfs {
		let giver_orig = info.get(i).unwrap();
		let mut giver_info = giver_orig.get();
		giver_info.curr_transfer_dir = 6;
		giver_info.curr_transfer_amount = 0.0;
		*queue_cycle_slow += 1;
		queue.clear();
		queue.insert((*i, *m));
		giver_info.last_slow_queue_cycle = *queue_cycle_slow;
		giver_orig.set(giver_info);
		let mut queue_idx = 0;
		while queue_idx < queue.len() {
			if giver_info.mole_delta <= 0.0 {
				break;
			}
			let (idx, turf) = queue[queue_idx];
			for (j, loc) in adjacent_tile_ids(turf.adjacency, idx, max_x, max_y) {
				if giver_info.mole_delta <= 0.0 {
					break;
				}
				if let Some(adj_orig) = info.get(&loc) {
					if let Some(adj_mix) = turf_gases().get(&loc) {
						let mut adj_info = adj_orig.get();
						if adj_info.last_slow_queue_cycle != *queue_cycle_slow {
							if queue.insert((loc, *adj_mix.value())) {
								adj_info.last_slow_queue_cycle = *queue_cycle_slow;
								adj_info.curr_transfer_dir = OPP_DIR_INDEX[j as usize];
								adj_info.curr_transfer_amount = 0.0;
								if adj_info.mole_delta < 0.0 {
									// this turf needs gas. Let's give it to 'em.
									if -adj_info.mole_delta > giver_info.mole_delta {
										// we don't have enough gas
										adj_info.curr_transfer_amount -= giver_info.mole_delta;
										adj_info.mole_delta += giver_info.mole_delta;
										giver_info.mole_delta = 0.0;
									} else {
										// we have enough gas.
										adj_info.curr_transfer_amount += adj_info.mole_delta;
										giver_info.mole_delta += adj_info.mole_delta;
										adj_info.mole_delta = 0.0;
									}
								}
								giver_orig.set(giver_info);
								adj_orig.set(adj_info);
							}
						}
					}
				}
			}
			queue_idx += 1;
		}
		for (idx, _) in queue.drain(..).rev() {
			let turf_orig = info.get(&idx).unwrap();
			let mut turf_info = turf_orig.get();
			if turf_info.curr_transfer_amount != 0.0 && turf_info.curr_transfer_dir != 6 {
				let adj_tile_id =
					adjacent_tile_id(turf_info.curr_transfer_dir as u8, idx, max_x, max_y);
				let adj_orig = info.get(&adj_tile_id).unwrap();
				let mut adj_info = adj_orig.get();
				turf_info.adjust_eq_movement(
					&mut adj_info,
					turf_info.curr_transfer_dir,
					turf_info.curr_transfer_amount,
				);
				adj_info.curr_transfer_amount += turf_info.curr_transfer_amount;
				turf_info.curr_transfer_amount = 0.0;
				turf_orig.set(turf_info);
				adj_orig.set(adj_info);
			}
		}
	}
}

fn take_from_givers(
	taker_turfs: &Vec<MixWithID>,
	giver_turfs: &Vec<MixWithID>,
	max_x: i32,
	max_y: i32,
	info: &HashMap<TurfID, Cell<MonstermosInfo>>,
	queue_cycle_slow: &mut i32,
) {
	let mut queue: IndexSet<MixWithID> = IndexSet::with_capacity(giver_turfs.len());
	for (i, m) in taker_turfs {
		let taker_orig = info.get(i).unwrap();
		let mut taker_info = taker_orig.get();
		taker_info.curr_transfer_dir = 6;
		taker_info.curr_transfer_amount = 0.0;
		*queue_cycle_slow += 1;
		queue.clear();
		queue.insert((*i, *m));
		taker_info.last_slow_queue_cycle = *queue_cycle_slow;
		taker_orig.set(taker_info);
		let mut queue_idx = 0;
		while queue_idx < queue.len() {
			if taker_info.mole_delta >= 0.0 {
				break;
			}
			let (idx, turf) = queue[queue_idx];
			for (j, loc) in adjacent_tile_ids(turf.adjacency, idx, max_x, max_y) {
				if taker_info.mole_delta >= 0.0 {
					break;
				}
				if let Some(adj_orig) = info.get(&loc) {
					if let Some(adj_mix) = turf_gases().get(&loc) {
						let mut adj_info = adj_orig.get();
						if adj_info.last_slow_queue_cycle != *queue_cycle_slow {
							if queue.insert((loc, *adj_mix)) {
								adj_info.last_slow_queue_cycle = *queue_cycle_slow;
								adj_info.curr_transfer_dir = OPP_DIR_INDEX[j as usize];
								adj_info.curr_transfer_amount = 0.0;
								if adj_info.mole_delta > 0.0 {
									// this turf has gas we can succ. Time to succ.
									if adj_info.mole_delta > -taker_info.mole_delta {
										// they have enough gase
										adj_info.curr_transfer_amount -= taker_info.mole_delta;
										adj_info.mole_delta += taker_info.mole_delta;
										taker_info.mole_delta = 0.0;
									} else {
										// they don't have neough gas
										adj_info.curr_transfer_amount += adj_info.mole_delta;
										taker_info.mole_delta += adj_info.mole_delta;
										adj_info.mole_delta = 0.0;
									}
								}
								adj_orig.set(adj_info);
								taker_orig.set(taker_info);
							}
						}
					}
				}
			}
			queue_idx += 1;
		}
		for (idx, _) in queue.drain(..).rev() {
			let turf_orig = info.get(&idx).unwrap();
			let mut turf_info = turf_orig.get();
			if turf_info.curr_transfer_amount != 0.0 && turf_info.curr_transfer_dir != 6 {
				let adj_orig = info
					.get(&adjacent_tile_id(
						turf_info.curr_transfer_dir as u8,
						idx,
						max_x,
						max_y,
					))
					.unwrap();
				let mut adj_info = adj_orig.get();
				turf_info.adjust_eq_movement(
					&mut adj_info,
					turf_info.curr_transfer_dir,
					turf_info.curr_transfer_amount,
				);
				adj_info.curr_transfer_amount += turf_info.curr_transfer_amount;
				turf_info.curr_transfer_amount = 0.0;
				turf_orig.set(turf_info);
				adj_orig.set(adj_info);
			}
		}
	}
}

fn process_planet_turfs(
	planet_turfs: &IndexSet<MixWithID>,
	average_moles: f32,
	max_x: i32,
	max_y: i32,
	info: &mut HashMap<TurfID, Cell<MonstermosInfo>>,
	mut queue_cycle_slow: i32,
) -> DMResult {
	let (_, sample_turf) = planet_turfs[0];
	let planet_sum = planetary_atmos()
		.get(&sample_turf.planetary_atmos.unwrap())
		.unwrap()
		.value()
		.total_moles();
	let target_delta = planet_sum - average_moles;
	queue_cycle_slow += 1;
	let mut progression_order: IndexSet<MixWithID> = IndexSet::with_capacity(planet_turfs.len());
	for (i, m) in planet_turfs.iter() {
		progression_order.insert((*i, *m));
		let mut cur_info = info.entry(*i).or_default().get_mut();
		cur_info.curr_transfer_dir = 6;
		cur_info.last_slow_queue_cycle = queue_cycle_slow;
	}
	// now build a map of where the path to a planet turf is for each tile.
	let mut queue_idx = 0;
	while queue_idx < progression_order.len() {
		let (i, m) = progression_order[queue_idx];
		for (j, loc) in adjacent_tile_ids(m.adjacency, i, max_x, max_y) {
			if let Some(adj_orig) = info.get(&loc) {
				let mut adj_info = adj_orig.get();
				if let Some(adj) = turf_gases().get(&loc) {
					if adj_info.last_slow_queue_cycle == queue_cycle_slow
							|| adj.value().planetary_atmos.is_some()
						{
							continue;
						}
					if progression_order.insert((*adj.key(), *adj.value())) {
						unsafe { Value::turf_by_id_unchecked(i as u32) }.call(
							"consider_firelocks",
							&[&unsafe { Value::turf_by_id_unchecked(loc as u32) }],
						)?;
						adj_info.last_slow_queue_cycle = queue_cycle_slow;
						adj_info.curr_transfer_dir = OPP_DIR_INDEX[j as usize];
						adj_orig.set(adj_info);
					}
				}
			}
		}
		queue_idx += 1;
	}
	for (i, _) in progression_order.iter().rev() {
		let cur_orig = info.get(i).unwrap();
		let mut cur_info = cur_orig.get();
		let airflow = cur_info.mole_delta - target_delta;
		let adj_orig = info
			.get(&adjacent_tile_id(
				cur_info.curr_transfer_dir as u8,
				*i,
				max_x,
				max_y,
			))
			.unwrap();
		let mut adj_info = adj_orig.get();
		cur_info.adjust_eq_movement(&mut adj_info, cur_info.curr_transfer_dir, airflow);
		if cur_info.curr_transfer_dir != 6 {
			adj_info.mole_delta += airflow;
		}
		cur_info.mole_delta = target_delta;
		cur_orig.set(cur_info);
		adj_orig.set(adj_info);
	}
	Ok(Value::null())
}

pub(crate) fn equalize(
	equalize_turf_limit: usize,
	equalize_hard_turf_limit: usize,
	max_x: i32,
	max_y: i32,
	high_pressure_turfs: BTreeSet<TurfID>,
) -> usize {
	let mut info: HashMap<TurfID, Cell<MonstermosInfo>> = HashMap::new();
	let mut turfs_processed = 0;
	let mut queue_cycle_slow = 1;
	let mut found_turfs: BTreeSet<TurfID> = BTreeSet::new();
	for &i in high_pressure_turfs.iter() {
		if found_turfs.contains(&i)
			|| turf_gases().get(&i).map_or(true, |m| {
				!m.enabled()
					|| m.adjacency <= 0 || GasArena::with_all_mixtures(|all_mixtures| {
					let our_moles = all_mixtures[m.mix].read().total_moles();
					our_moles < 10.0
						|| m.adjacent_mixes(all_mixtures).all(|lock| {
							(lock.read().total_moles() - our_moles).abs()
								< MINIMUM_MOLES_DELTA_TO_MOVE
						})
				})
			}) {
			continue;
		}
		let m = turf_gases().get(&i).unwrap();
		let maybe_turfs = flood_fill_equalize_turfs(
			i,
			*m,
			equalize_turf_limit,
			equalize_hard_turf_limit,
			max_x,
			max_y,
			&mut found_turfs,
			&mut info,
		);
		if maybe_turfs.is_none() {
			continue;
		}
		let (mut turfs, planet_turfs, total_moles) = maybe_turfs.unwrap();
		if turfs.len() > equalize_turf_limit {
			// throw out any above turf limit, we check more for explosive decomp
			for (idx, _) in turfs.drain(equalize_turf_limit..) {
				found_turfs.remove(&idx);
			}
		}
		let average_moles = (total_moles / (turfs.len() - planet_turfs.len()) as f64) as f32;
		let (mut giver_turfs, mut taker_turfs): (Vec<_>, Vec<_>) =
			turfs.iter().partition(|&(i, m)| {
				let cur_info = info.entry(*i).or_default().get_mut();
				cur_info.mole_delta = m.total_moles() - average_moles;
				cur_info.mole_delta > 0.0
			});
		let log_n = ((turfs.len() as f32).log2().floor()) as usize;
		if giver_turfs.len() > log_n && taker_turfs.len() > log_n {
			turfs.sort_by(|idx, idy| {
				let (x, _) = idx;
				let (y, _) = idy;
				float_ord::FloatOrd(info.get(x).unwrap().get().mole_delta)
					.cmp(&float_ord::FloatOrd(info.get(y).unwrap().get().mole_delta)).reverse()
			});
			for &(i, m) in &turfs {
				monstermos_fast_process(i, m, max_x, max_y, &mut info);
			}
			giver_turfs.clear();
			taker_turfs.clear();
			for &(i, m) in &turfs {
				if info.entry(i).or_default().get().mole_delta > 0.0 {
					giver_turfs.push((i, m));
				} else {
					taker_turfs.push((i, m));
				}
			}
		}
		// alright this is the part that can become O(n^2).
		if giver_turfs.len() < taker_turfs.len() {
			// as an optimization, we choose one of two methods based on which list is smaller.
			give_to_takers(
				&giver_turfs,
				&taker_turfs,
				max_x,
				max_y,
				&info,
				&mut queue_cycle_slow,
			);
		} else {
			take_from_givers(
				&taker_turfs,
				&giver_turfs,
				max_x,
				max_y,
				&info,
				&mut queue_cycle_slow,
			);
		}
		if !planet_turfs.is_empty() {
			turfs_processed += turfs.len() + planet_turfs.len();
			let sender = byond_callback_sender();
			let fake_cloned = info
				.iter()
				.map(|(&k, v)| (k, v.get()))
				.collect::<HashMap<TurfID, MonstermosInfo>>();
			let _ = sender.send(Box::new(move || {
				let mut cloned = fake_cloned
					.iter()
					.map(|(&k, &v)| (k, Cell::new(v)))
					.collect::<HashMap<TurfID, Cell<MonstermosInfo>>>();
				process_planet_turfs(
					&planet_turfs,
					average_moles,
					max_x,
					max_y,
					&mut cloned,
					queue_cycle_slow,
				)?;
				for (i, turf) in turfs.iter() {
					finalize_eq(*i, turf, &cloned, max_x, max_y);
				}
				Ok(Value::null())
			}));
		} else {
			turfs_processed += turfs.len();
			for (i, turf) in turfs.iter() {
				finalize_eq(*i, turf, &info, max_x, max_y);
			}
		}
	}
	turfs_processed
}
