// Copyright 2018 Google LLC, licensed under http://www.apache.org/licenses/LICENSE-2.0

use control::ControlMap;
use dimensioned::si;
use draw_car::DrawCar;
use draw_ped::DrawPedestrian;
use driving::DrivingSimState;
use map_model;
use map_model::{LaneID, LaneType, Map, Turn, TurnID};
use parking::ParkingSimState;
use rand::{FromEntropy, Rng, SeedableRng, XorShiftRng};
use std::collections::VecDeque;
use std::f64;
use std::time::{Duration, Instant};
use walking::WalkingSimState;
use {pick_goal_and_find_path, CarID, PedestrianID, Tick, TIMESTEP};

pub enum CarState {
    Moving,
    Stuck,
    Parked,
}

#[derive(Serialize, Deserialize, Derivative)]
#[derivative(PartialEq, Eq)]
pub struct Sim {
    // This is slightly dangerous, but since we'll be using comparisons based on savestating (which
    // captures the RNG), this should be OK for now.
    #[derivative(PartialEq = "ignore")]
    rng: XorShiftRng,
    pub time: Tick,
    car_id_counter: usize,
    debug: Option<CarID>,

    driving_state: DrivingSimState,
    parking_state: ParkingSimState,
    walking_state: WalkingSimState,
}

impl Sim {
    pub fn new(map: &Map, rng_seed: Option<u8>) -> Sim {
        let mut rng = XorShiftRng::from_entropy();
        if let Some(seed) = rng_seed {
            rng = XorShiftRng::from_seed([seed; 16]);
        }

        Sim {
            rng,
            driving_state: DrivingSimState::new(map),
            parking_state: ParkingSimState::new(map),
            walking_state: WalkingSimState::new(),
            time: Tick::zero(),
            car_id_counter: 0,
            debug: None,
        }
    }

    pub fn edit_lane_type(&mut self, id: LaneID, old_type: LaneType, map: &Map) {
        match old_type {
            LaneType::Driving => self.driving_state.edit_remove_lane(id),
            LaneType::Parking => self.parking_state.edit_remove_lane(id),
            LaneType::Sidewalk => self.walking_state.edit_remove_lane(id),
            LaneType::Biking => {}
        };
        let l = map.get_l(id);
        match l.lane_type {
            LaneType::Driving => self.driving_state.edit_add_lane(id),
            LaneType::Parking => self.parking_state.edit_add_lane(l),
            LaneType::Sidewalk => self.walking_state.edit_add_lane(id),
            LaneType::Biking => {}
        };
    }

    pub fn edit_remove_turn(&mut self, t: &Turn) {
        if t.between_sidewalks {
            self.walking_state.edit_remove_turn(t.id);
        } else {
            self.driving_state.edit_remove_turn(t.id);
        }
    }

    pub fn edit_add_turn(&mut self, t: &Turn, map: &Map) {
        if t.between_sidewalks {
            self.walking_state.edit_add_turn(t.id);
        } else {
            self.driving_state.edit_add_turn(t.id, map);
        }
    }

    pub fn total_cars(&self) -> usize {
        self.car_id_counter
    }

    pub fn seed_parked_cars(&mut self, percent: f64) {
        self.parking_state
            .seed_random_cars(&mut self.rng, percent, &mut self.car_id_counter)
    }

    pub fn start_many_parked_cars(&mut self, map: &Map, num_cars: usize) {
        let mut driving_lanes = self.driving_state.get_empty_lanes();
        // Don't ruin determinism for silly reasons. :)
        if !driving_lanes.is_empty() {
            self.rng.shuffle(&mut driving_lanes);
        }

        let n = num_cars.min(driving_lanes.len());
        let mut actual = 0;
        for i in 0..n {
            if self.start_agent(map, driving_lanes[i]) {
                actual += 1;
            }
        }
        println!("Started {} parked cars of requested {}", actual, n);
    }

    pub fn start_agent(&mut self, map: &Map, id: LaneID) -> bool {
        // TODO maybe a way to grab both?
        let lane = map.get_l(id);
        let road = map.get_r(lane.parent);
        let (driving_lane, parking_lane) = match lane.lane_type {
            LaneType::Sidewalk => {
                if let Some(path) = pick_goal_and_find_path(&mut self.rng, map, id) {
                    println!("Spawned a pedestrian at {}", id);
                    self.walking_state.seed_pedestrian(map, path);
                    return true;
                } else {
                    return false;
                }
            }
            LaneType::Driving => {
                if let Some(parking) = road.find_parking_lane(id) {
                    (id, parking)
                } else {
                    println!("{} has no parking lane", id);
                    return false;
                }
            }
            LaneType::Parking => {
                if let Some(driving) = road.find_driving_lane(id) {
                    (driving, id)
                } else {
                    println!("{} has no driving lane", id);
                    return false;
                }
            }
            LaneType::Biking => {
                println!("TODO implement bikes");
                return false;
            }
        };

        if let Some(car) = self.parking_state.get_last_parked_car(parking_lane) {
            if self.driving_state.start_car_on_lane(
                self.time,
                driving_lane,
                car,
                map,
                &mut self.rng,
            ) {
                self.parking_state.remove_last_parked_car(parking_lane, car);
            }
            true
        } else {
            println!("No parked cars on {}", parking_lane);
            false
        }
    }

    pub fn seed_pedestrians(&mut self, map: &Map, num: usize) {
        use rayon::prelude::*;

        let mut sidewalks: Vec<LaneID> = Vec::new();
        for l in map.all_lanes() {
            if l.lane_type == LaneType::Sidewalk {
                sidewalks.push(l.id);
            }
        }

        let mut requested_paths: Vec<(LaneID, LaneID)> = Vec::new();
        for _i in 0..num {
            let start = *self.rng.choose(&sidewalks).unwrap();
            let goal = choose_different(&mut self.rng, &sidewalks, start);
            requested_paths.push((start, goal));
        }

        println!("Calculating {} paths for pedestrians", num);
        // TODO better timer macro
        let timer = Instant::now();
        let paths: Vec<Option<Vec<LaneID>>> = requested_paths
            .par_iter()
            .map(|(start, goal)| map_model::pathfind(map, *start, *goal))
            .collect();

        let mut actual = 0;
        for path in paths.into_iter() {
            if let Some(steps) = path {
                self.walking_state
                    .seed_pedestrian(map, VecDeque::from(steps));
                actual += 1;
            } else {
                // zip with request to have start/goal?
                //println!("Failed to pathfind for a pedestrian");
            };
        }

        println!(
            "Calculating {} pedestrian paths took {:?}",
            num,
            timer.elapsed()
        );
        println!("Spawned {} pedestrians of requested {}", actual, num);
    }

    pub fn step(&mut self, map: &Map, control_map: &ControlMap) {
        self.time.increment();

        // TODO Vanish action should become Park
        self.driving_state.step(self.time, map, control_map);
        self.walking_state.step(TIMESTEP, map, control_map);
    }

    pub fn get_car_state(&self, c: CarID) -> CarState {
        if let Some(driving) = self.driving_state.cars.get(&c) {
            if driving.waiting_for.is_none() {
                CarState::Moving
            } else {
                CarState::Stuck
            }
        } else {
            CarState::Parked
        }
    }

    // TODO maybe just DrawAgent instead? should caller care?
    pub fn get_draw_cars_on_lane(&self, l: LaneID, map: &Map) -> Vec<DrawCar> {
        match map.get_l(l).lane_type {
            LaneType::Driving => self.driving_state.get_draw_cars_on_lane(l, self.time, map),
            LaneType::Parking => self.parking_state.get_draw_cars(l, map),
            LaneType::Sidewalk => Vec::new(),
            LaneType::Biking => Vec::new(),
        }
    }

    pub fn get_draw_cars_on_turn(&self, t: TurnID, map: &Map) -> Vec<DrawCar> {
        self.driving_state.get_draw_cars_on_turn(t, self.time, map)
    }

    pub fn get_draw_peds_on_lane(&self, l: LaneID, map: &Map) -> Vec<DrawPedestrian> {
        self.walking_state.get_draw_peds_on_lane(map.get_l(l))
    }

    pub fn get_draw_peds_on_turn(&self, t: TurnID, map: &Map) -> Vec<DrawPedestrian> {
        self.walking_state.get_draw_peds_on_turn(map.get_t(t))
    }

    pub fn summary(&self) -> String {
        // TODO also report parking state and walking state
        let waiting = self.driving_state
            .cars
            .values()
            .filter(|c| c.waiting_for.is_some())
            .count();
        format!(
            "Time: {0:.2}, {1} / {2} active cars waiting, {3} cars parked, {4} pedestrians",
            self.time,
            waiting,
            self.driving_state.cars.len(),
            self.parking_state.total_count(),
            self.walking_state.total_count(),
        )
    }

    pub fn ped_tooltip(&self, p: PedestrianID) -> Vec<String> {
        vec![format!("Hello to {}", p)]
    }

    pub fn car_tooltip(&self, car: CarID) -> Vec<String> {
        if let Some(driving) = self.driving_state.cars.get(&car) {
            driving.tooltip_lines()
        } else {
            vec![format!("{} is parked", car)]
        }
    }

    pub fn toggle_debug(&mut self, car: CarID) {
        if let Some(c) = self.debug {
            if c != car {
                self.driving_state.cars.get_mut(&c).unwrap().debug = false;
            }
        }

        let c = self.driving_state.cars.get_mut(&car).unwrap();
        c.debug = !c.debug;
        self.debug = Some(car);
    }

    pub fn start_benchmark(&self) -> Benchmark {
        Benchmark {
            last_real_time: Instant::now(),
            last_sim_time: self.time,
        }
    }

    pub fn measure_speed(&self, b: &mut Benchmark) -> f64 {
        let elapsed = b.last_real_time.elapsed();
        let dt = (elapsed.as_secs() as f64 + f64::from(elapsed.subsec_nanos()) * 1e-9) * si::S;
        let speed = (self.time - b.last_sim_time).as_time() / dt;
        b.last_real_time = Instant::now();
        b.last_sim_time = self.time;
        speed.value_unsafe
    }
}

pub struct Benchmark {
    last_real_time: Instant,
    last_sim_time: Tick,
}

impl Benchmark {
    pub fn has_real_time_passed(&self, d: Duration) -> bool {
        self.last_real_time.elapsed() >= d
    }
}

fn choose_different<R: Rng + ?Sized, T: PartialEq + Copy>(
    rng: &mut R,
    choices: &Vec<T>,
    except: T,
) -> T {
    assert!(choices.len() > 1);
    loop {
        let choice = *rng.choose(choices).unwrap();
        if choice != except {
            return choice;
        }
    }
}
