use crate::plugins::sim::new_des_model::{
    DrivingGoal, ParkingSpot, SidewalkSpot, Sim, TripSpec, VehicleSpec, MAX_BIKE_LENGTH,
    MAX_CAR_LENGTH, MIN_BIKE_LENGTH, MIN_CAR_LENGTH,
};
use abstutil;
use abstutil::{fork_rng, Timer, WeightedUsizeChoice};
use geom::{Distance, Duration, Speed};
use map_model::{
    BuildingID, FullNeighborhoodInfo, IntersectionID, LaneType, Map, Pathfinder, Position, RoadID,
};
use rand::seq::SliceRandom;
use rand::Rng;
use rand_xorshift::XorShiftRng;
use serde_derive::{Deserialize, Serialize};
use sim::{CarID, VehicleType};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Scenario {
    pub scenario_name: String,
    pub map_name: String,

    pub seed_parked_cars: Vec<SeedParkedCars>,
    pub spawn_over_time: Vec<SpawnOverTime>,
    pub border_spawn_over_time: Vec<BorderSpawnOverTime>,
}

// SpawnOverTime and BorderSpawnOverTime should be kept separate. Agents in SpawnOverTime pick
// their mode (use a car, walk, bus) based on the situation. When spawning directly a border,
// agents have to start as a car or pedestrian already.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct SpawnOverTime {
    pub num_agents: usize,
    // TODO use https://docs.rs/rand/0.5.5/rand/distributions/struct.Normal.html
    pub start_time: Duration,
    pub stop_time: Duration,
    pub start_from_neighborhood: String,
    pub goal: OriginDestination,
    pub percent_biking: f64,
    pub percent_use_transit: f64,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct BorderSpawnOverTime {
    pub num_peds: usize,
    pub num_cars: usize,
    pub num_bikes: usize,
    // TODO use https://docs.rs/rand/0.5.5/rand/distributions/struct.Normal.html
    pub start_time: Duration,
    pub stop_time: Duration,
    // TODO A serialized Scenario won't last well as the map changes...
    pub start_from_border: IntersectionID,
    pub goal: OriginDestination,
    pub percent_use_transit: f64,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct SeedParkedCars {
    pub neighborhood: String,
    pub cars_per_building: WeightedUsizeChoice,
}

impl Scenario {
    pub fn describe(&self) -> Vec<String> {
        abstutil::to_json(self)
            .split('\n')
            .map(|s| s.to_string())
            .collect()
    }

    // TODO may need to fork the RNG a bit more
    pub fn instantiate(&self, sim: &mut Sim, map: &Map, rng: &mut XorShiftRng, timer: &mut Timer) {
        timer.start(&format!("Instantiating {}", self.scenario_name));

        timer.start("load full neighborhood info");
        let neighborhoods = FullNeighborhoodInfo::load_all(map);
        timer.stop("load full neighborhood info");

        for s in &self.seed_parked_cars {
            if !neighborhoods.contains_key(&s.neighborhood) {
                panic!("Neighborhood {} isn't defined", s.neighborhood);
            }

            seed_parked_cars(
                sim,
                &s.cars_per_building,
                &neighborhoods[&s.neighborhood].buildings,
                &neighborhoods[&s.neighborhood].roads,
                rng,
                map,
                timer,
            );
        }

        // Don't let two pedestrians starting from one building use the same car.
        let mut reserved_cars: HashSet<CarID> = HashSet::new();
        for s in &self.spawn_over_time {
            if !neighborhoods.contains_key(&s.start_from_neighborhood) {
                panic!("Neighborhood {} isn't defined", s.start_from_neighborhood);
            }

            timer.start_iter("SpawnOverTime each agent", s.num_agents);
            for _ in 0..s.num_agents {
                timer.next();
                let spawn_time = rand_time(rng, s.start_time, s.stop_time);
                // Note that it's fine for agents to start/end at the same building. Later we might
                // want a better assignment of people per household, or workers per office building.
                let from_bldg = *neighborhoods[&s.start_from_neighborhood]
                    .buildings
                    .choose(rng)
                    .unwrap();

                // What mode?
                if let Some(parked_car) = sim
                    .get_parked_cars_by_owner(from_bldg)
                    .into_iter()
                    .find(|p| !reserved_cars.contains(&p.vehicle.id))
                {
                    if let Some(goal) = s.goal.pick_driving_goal(map, &neighborhoods, rng, timer) {
                        reserved_cars.insert(parked_car.vehicle.id);
                        sim.schedule_trip(
                            spawn_time,
                            TripSpec::UsingParkedCar(
                                SidewalkSpot::building(from_bldg, map),
                                parked_car.spot,
                                goal,
                            ),
                            map,
                        );
                    }
                } else if rng.gen_bool(s.percent_biking) {
                    if let Some(goal) = s.goal.pick_biking_goal(map, &neighborhoods, rng, timer) {
                        let skip = if let DrivingGoal::ParkNear(to_bldg) = goal {
                            map.get_b(to_bldg).sidewalk() == map.get_b(from_bldg).sidewalk()
                        } else {
                            false
                        };

                        if !skip {
                            sim.schedule_trip(
                                spawn_time,
                                TripSpec::UsingBike(
                                    SidewalkSpot::building(from_bldg, map),
                                    rand_bike(rng),
                                    goal,
                                ),
                                map,
                            );
                        }
                    }
                } else if let Some(goal) = s.goal.pick_walking_goal(map, &neighborhoods, rng, timer)
                {
                    let start_spot = SidewalkSpot::building(from_bldg, map);

                    if rng.gen_bool(s.percent_use_transit) {
                        // TODO This throws away some work. It also sequentially does expensive
                        // work right here.
                        if let Some((stop1, stop2, route)) = Pathfinder::should_use_transit(
                            map,
                            start_spot.sidewalk_pos,
                            goal.sidewalk_pos,
                        ) {
                            sim.schedule_trip(
                                spawn_time,
                                TripSpec::UsingTransit(start_spot, route, stop1, stop2, goal),
                                map,
                            );
                            continue;
                        }
                    }

                    sim.schedule_trip(spawn_time, TripSpec::JustWalking(start_spot, goal), map);
                }
            }
        }

        timer.start_iter("BorderSpawnOverTime", self.border_spawn_over_time.len());
        for s in &self.border_spawn_over_time {
            timer.next();
            if let Some(start) = SidewalkSpot::start_at_border(s.start_from_border, map) {
                for _ in 0..s.num_peds {
                    let spawn_time = rand_time(rng, s.start_time, s.stop_time);
                    if let Some(goal) = s.goal.pick_walking_goal(map, &neighborhoods, rng, timer) {
                        if rng.gen_bool(s.percent_use_transit) {
                            // TODO This throws away some work. It also sequentially does expensive
                            // work right here.
                            if let Some((stop1, stop2, route)) = Pathfinder::should_use_transit(
                                map,
                                start.sidewalk_pos,
                                goal.sidewalk_pos,
                            ) {
                                sim.schedule_trip(
                                    spawn_time,
                                    TripSpec::UsingTransit(
                                        start.clone(),
                                        route,
                                        stop1,
                                        stop2,
                                        goal,
                                    ),
                                    map,
                                );
                                continue;
                            }
                        }

                        sim.schedule_trip(
                            spawn_time,
                            TripSpec::JustWalking(start.clone(), goal),
                            map,
                        );
                    }
                }
            } else if s.num_peds > 0 {
                timer.warn(format!(
                    "Can't start_at_border for {} without sidewalk",
                    s.start_from_border
                ));
            }

            let starting_driving_lanes = map
                .get_i(s.start_from_border)
                .get_outgoing_lanes(map, LaneType::Driving);
            if !starting_driving_lanes.is_empty() {
                let lane_len = map.get_l(starting_driving_lanes[0]).length();
                if lane_len < MAX_CAR_LENGTH {
                    timer.warn(format!(
                        "Skipping {:?} because {} is only {}, too short to spawn cars",
                        s, starting_driving_lanes[0], lane_len
                    ));
                } else {
                    for _ in 0..s.num_cars {
                        let spawn_time = rand_time(rng, s.start_time, s.stop_time);
                        if let Some(goal) =
                            s.goal.pick_driving_goal(map, &neighborhoods, rng, timer)
                        {
                            // TODO Do the distance correction here
                            sim.schedule_trip(
                                spawn_time,
                                TripSpec::CarAppearing(
                                    // TODO could pretty easily pick any lane here
                                    Position::new(starting_driving_lanes[0], Distance::ZERO),
                                    rand_car(rng),
                                    goal,
                                ),
                                map,
                            );
                        }
                    }
                }
            } else if s.num_cars > 0 {
                timer.warn(format!(
                    "Can't start car at border for {}",
                    s.start_from_border
                ));
            }

            let mut starting_biking_lanes = map
                .get_i(s.start_from_border)
                .get_outgoing_lanes(map, LaneType::Biking);
            for l in starting_driving_lanes {
                if map.get_parent(l).supports_bikes() {
                    starting_biking_lanes.push(l);
                }
            }
            if !starting_biking_lanes.is_empty() {
                for _ in 0..s.num_bikes {
                    let spawn_time = rand_time(rng, s.start_time, s.stop_time);
                    if let Some(goal) = s.goal.pick_biking_goal(map, &neighborhoods, rng, timer) {
                        let bike = rand_bike(rng);
                        sim.schedule_trip(
                            spawn_time,
                            TripSpec::CarAppearing(
                                Position::new(starting_biking_lanes[0], bike.length),
                                bike,
                                goal,
                            ),
                            map,
                        );
                    }
                }
            } else if s.num_bikes > 0 {
                timer.warn(format!(
                    "Can't start bike at border for {}",
                    s.start_from_border
                ));
            }
        }

        sim.spawn_all_trips(map);
        timer.stop(&format!("Instantiating {}", self.scenario_name));
    }

    pub fn save(&self) {
        abstutil::save_object("scenarios", &self.map_name, &self.scenario_name, self);
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum OriginDestination {
    Neighborhood(String),
    // TODO A serialized Scenario won't last well as the map changes...
    Border(IntersectionID),
}

impl OriginDestination {
    fn pick_driving_goal(
        &self,
        map: &Map,
        neighborhoods: &HashMap<String, FullNeighborhoodInfo>,
        rng: &mut XorShiftRng,
        timer: &mut Timer,
    ) -> Option<DrivingGoal> {
        match self {
            OriginDestination::Neighborhood(ref n) => Some(DrivingGoal::ParkNear(
                *neighborhoods[n].buildings.choose(rng).unwrap(),
            )),
            OriginDestination::Border(i) => {
                let lanes = map.get_i(*i).get_incoming_lanes(map, LaneType::Driving);
                if lanes.is_empty() {
                    timer.warn(format!(
                        "Can't spawn a car ending at border {}; no driving lane there",
                        i
                    ));
                    None
                } else {
                    // TODO ideally could use any
                    Some(DrivingGoal::Border(*i, lanes[0]))
                }
            }
        }
    }

    // TODO nearly a copy of pick_driving_goal! Ew
    fn pick_biking_goal(
        &self,
        map: &Map,
        neighborhoods: &HashMap<String, FullNeighborhoodInfo>,
        rng: &mut XorShiftRng,
        timer: &mut Timer,
    ) -> Option<DrivingGoal> {
        match self {
            OriginDestination::Neighborhood(ref n) => Some(DrivingGoal::ParkNear(
                *neighborhoods[n].buildings.choose(rng).unwrap(),
            )),
            OriginDestination::Border(i) => {
                let mut lanes = map.get_i(*i).get_incoming_lanes(map, LaneType::Biking);
                lanes.extend(map.get_i(*i).get_incoming_lanes(map, LaneType::Driving));
                if lanes.is_empty() {
                    timer.warn(format!(
                        "Can't spawn a bike ending at border {}; no biking or driving lane there",
                        i
                    ));
                    None
                } else {
                    Some(DrivingGoal::Border(*i, lanes[0]))
                }
            }
        }
    }

    fn pick_walking_goal(
        &self,
        map: &Map,
        neighborhoods: &HashMap<String, FullNeighborhoodInfo>,
        rng: &mut XorShiftRng,
        timer: &mut Timer,
    ) -> Option<SidewalkSpot> {
        match self {
            OriginDestination::Neighborhood(ref n) => Some(SidewalkSpot::building(
                *neighborhoods[n].buildings.choose(rng).unwrap(),
                map,
            )),
            OriginDestination::Border(i) => {
                let goal = SidewalkSpot::end_at_border(*i, map);
                if goal.is_none() {
                    timer.warn(format!("Can't end_at_border for {} without a sidewalk", i));
                }
                goal
            }
        }
    }
}

fn seed_parked_cars(
    sim: &mut Sim,
    cars_per_building: &WeightedUsizeChoice,
    owner_buildings: &Vec<BuildingID>,
    neighborhoods_roads: &BTreeSet<RoadID>,
    base_rng: &mut XorShiftRng,
    map: &Map,
    timer: &mut Timer,
) {
    // Track the available parking spots per road, only for the roads in the appropriate
    // neighborhood.
    let mut total_spots = 0;
    let mut open_spots_per_road: HashMap<RoadID, Vec<ParkingSpot>> = HashMap::new();
    for id in neighborhoods_roads {
        let r = map.get_r(*id);
        let mut spots: Vec<ParkingSpot> = Vec::new();
        for (lane, lane_type) in r
            .children_forwards
            .iter()
            .chain(r.children_backwards.iter())
        {
            if *lane_type == LaneType::Parking {
                spots.extend(sim.get_free_spots(*lane));
            }
        }
        total_spots += spots.len();
        spots.shuffle(&mut fork_rng(base_rng));
        open_spots_per_road.insert(r.id, spots);
    }

    let mut new_cars = 0;
    timer.start_iter("seed parked cars for buildings", owner_buildings.len());
    for b in owner_buildings {
        timer.next();
        for _ in 0..cars_per_building.sample(base_rng) {
            let mut forked_rng = fork_rng(base_rng);
            if let Some(spot) = find_spot_near_building(
                *b,
                &mut open_spots_per_road,
                neighborhoods_roads,
                map,
                timer,
            ) {
                sim.seed_parked_car(rand_car(&mut forked_rng), spot, Some(*b));
                new_cars += 1;
            } else {
                // TODO This should be more critical, but neighborhoods can currently contain a
                // building, but not even its road, so this is inevitable.
                timer.warn(format!(
                    "No room to seed parked cars. {} total spots, {:?} of {} buildings requested, {} new cars so far. Searched from {}",
                    total_spots,
                    cars_per_building,
                    owner_buildings.len(),
                    new_cars,
                    b
                ));
            }
        }
    }

    timer.note(format!(
        "Seeded {} of {} parking spots with cars, leaving {} buildings without cars",
        new_cars,
        total_spots,
        owner_buildings.len() - new_cars
    ));
}

// Pick a parking spot for this building. If the building's road has a free spot, use it. If not,
// start BFSing out from the road in a deterministic way until finding a nearby road with an open
// spot.
fn find_spot_near_building(
    b: BuildingID,
    open_spots_per_road: &mut HashMap<RoadID, Vec<ParkingSpot>>,
    neighborhoods_roads: &BTreeSet<RoadID>,
    map: &Map,
    timer: &mut Timer,
) -> Option<ParkingSpot> {
    let mut roads_queue: VecDeque<RoadID> = VecDeque::new();
    let mut visited: HashSet<RoadID> = HashSet::new();
    {
        let start = map.building_to_road(b).id;
        roads_queue.push_back(start);
        visited.insert(start);
    }

    loop {
        if roads_queue.is_empty() {
            timer.warn(format!(
                "Giving up looking for a free parking spot, searched {} roads of {}: {:?}",
                visited.len(),
                open_spots_per_road.len(),
                visited
            ));
        }
        let r = roads_queue.pop_front()?;
        if let Some(spots) = open_spots_per_road.get_mut(&r) {
            // TODO With some probability, skip this available spot and park farther away
            if !spots.is_empty() {
                return spots.pop();
            }
        }

        for next_r in map.get_next_roads(r).into_iter() {
            // Don't floodfill out of the neighborhood
            if !visited.contains(&next_r) && neighborhoods_roads.contains(&next_r) {
                roads_queue.push_back(next_r);
                visited.insert(next_r);
            }
        }
    }
}

fn rand_dist(rng: &mut XorShiftRng, low: Distance, high: Distance) -> Distance {
    Distance::meters(rng.gen_range(low.inner_meters(), high.inner_meters()))
}

fn rand_time(rng: &mut XorShiftRng, low: Duration, high: Duration) -> Duration {
    Duration::seconds(rng.gen_range(low.inner_seconds(), high.inner_seconds()))
}

fn rand_car(rng: &mut XorShiftRng) -> VehicleSpec {
    let length = rand_dist(rng, MIN_CAR_LENGTH, MAX_CAR_LENGTH);
    VehicleSpec {
        vehicle_type: VehicleType::Car,
        length,
        max_speed: None,
    }
}

fn rand_bike(rng: &mut XorShiftRng) -> VehicleSpec {
    let length = rand_dist(rng, MIN_BIKE_LENGTH, MAX_BIKE_LENGTH);
    let max_speed = Some(Speed::miles_per_hour(10.0));
    VehicleSpec {
        vehicle_type: VehicleType::Bus,
        length,
        max_speed,
    }
}
