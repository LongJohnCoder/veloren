use crate::{
    audio::sfx::{SfxEvent, SfxEventItem},
    ecs::MyEntity,
    hud::{DebugInfo, Event as HudEvent, Hud, HudInfo, PressBehavior},
    i18n::{i18n_asset_key, VoxygenLocalization},
    key_state::KeyState,
    menu::char_selection::CharSelectionState,
    render::Renderer,
    scene::{camera, CameraMode, Scene, SceneData},
    settings::{AudioOutput, ControlSettings, Settings},
    window::{AnalogGameInput, Event, GameInput},
    Direction, Error, GlobalState, PlayState, PlayStateResult,
};
use client::{self, Client};
use common::{
    assets::Asset,
    comp,
    comp::{
        ChatMsg, ChatType, InventoryUpdateEvent, Pos, Vel, MAX_MOUNT_RANGE_SQR,
        MAX_PICKUP_RANGE_SQR,
    },
    event::EventBus,
    outcome::Outcome,
    span,
    terrain::{Block, BlockKind},
    util::Dir,
    vol::ReadVol,
};
use specs::{Join, WorldExt};
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};
use tracing::{error, info};
use vek::*;

/// The action to perform after a tick
enum TickAction {
    // Continue executing
    Continue,
    // Disconnected (i.e. go to main menu)
    Disconnect,
}

pub struct SessionState {
    scene: Scene,
    client: Rc<RefCell<Client>>,
    hud: Hud,
    key_state: KeyState,
    inputs: comp::ControllerInputs,
    selected_block: Block,
    voxygen_i18n: std::sync::Arc<VoxygenLocalization>,
    walk_forward_dir: Vec2<f32>,
    walk_right_dir: Vec2<f32>,
    freefly_vel: Vec3<f32>,
    free_look: bool,
    auto_walk: bool,
    is_aiming: bool,
    target_entity: Option<specs::Entity>,
    selected_entity: Option<(specs::Entity, std::time::Instant)>,
}

/// Represents an active game session (i.e., the one being played).
impl SessionState {
    /// Create a new `SessionState`.
    pub fn new(global_state: &mut GlobalState, client: Rc<RefCell<Client>>) -> Self {
        // Create a scene for this session. The scene handles visible elements of the
        // game world.
        let mut scene = Scene::new(
            global_state.window.renderer_mut(),
            &*client.borrow(),
            &global_state.settings,
        );
        scene
            .camera_mut()
            .set_fov_deg(global_state.settings.graphics.fov);
        let hud = Hud::new(global_state, &client.borrow());
        let voxygen_i18n = VoxygenLocalization::load_expect(&i18n_asset_key(
            &global_state.settings.language.selected_language,
        ));

        let walk_forward_dir = scene.camera().forward_xy();
        let walk_right_dir = scene.camera().right_xy();

        Self {
            scene,
            client,
            key_state: KeyState::default(),
            inputs: comp::ControllerInputs::default(),
            hud,
            selected_block: Block::new(BlockKind::Misc, Rgb::broadcast(255)),
            voxygen_i18n,
            walk_forward_dir,
            walk_right_dir,
            freefly_vel: Vec3::zero(),
            free_look: false,
            auto_walk: false,
            is_aiming: false,
            target_entity: None,
            selected_entity: None,
        }
    }

    fn stop_auto_walk(&mut self) {
        self.auto_walk = false;
        self.hud.auto_walk(false);
        self.key_state.auto_walk = false;
    }

    /// Tick the session (and the client attached to it).
    fn tick(
        &mut self,
        dt: Duration,
        global_state: &mut GlobalState,
        outcomes: &mut Vec<Outcome>,
    ) -> Result<TickAction, Error> {
        span!(_guard, "tick", "Session::tick");
        self.inputs.tick(dt);

        let mut client = self.client.borrow_mut();
        for event in client.tick(self.inputs.clone(), dt, crate::ecs::sys::add_local_systems)? {
            match event {
                client::Event::Chat(m) => {
                    self.hud.new_message(m);
                },
                client::Event::InventoryUpdated(inv_event) => {
                    let sfx_event = SfxEvent::from(&inv_event);
                    client
                        .state()
                        .ecs()
                        .read_resource::<EventBus<SfxEventItem>>()
                        .emit_now(SfxEventItem::at_player_position(sfx_event));

                    match inv_event {
                        InventoryUpdateEvent::CollectFailed => {
                            self.hud.new_message(ChatMsg {
                                message: self.voxygen_i18n.get("hud.chat.loot_fail").to_string(),
                                chat_type: ChatType::CommandError,
                            });
                        },
                        InventoryUpdateEvent::Collected(item) => {
                            self.hud.new_message(ChatMsg {
                                message: self
                                    .voxygen_i18n
                                    .get("hud.chat.loot_msg")
                                    .replace("{item}", item.name()),
                                chat_type: ChatType::Loot,
                            });
                        },
                        _ => {},
                    };
                },
                client::Event::Disconnect => return Ok(TickAction::Disconnect),
                client::Event::DisconnectionNotification(time) => {
                    let message = match time {
                        0 => String::from(self.voxygen_i18n.get("hud.chat.goodbye")),
                        _ => self
                            .voxygen_i18n
                            .get("hud.chat.connection_lost")
                            .replace("{time}", time.to_string().as_str()),
                    };

                    self.hud.new_message(ChatMsg {
                        chat_type: ChatType::CommandError,
                        message,
                    });
                },
                client::Event::Kicked(reason) => {
                    global_state.info_message = Some(format!(
                        "{}: {}",
                        self.voxygen_i18n.get("main.login.kicked").to_string(),
                        reason
                    ));
                    return Ok(TickAction::Disconnect);
                },
                client::Event::Notification(n) => {
                    self.hud.new_notification(n);
                },
                client::Event::SetViewDistance(vd) => {
                    global_state.settings.graphics.view_distance = vd;
                    global_state.settings.save_to_file_warn();
                },
                client::Event::Outcome(outcome) => outcomes.push(outcome),
            }
        }

        Ok(TickAction::Continue)
    }

    /// Clean up the session (and the client attached to it) after a tick.
    pub fn cleanup(&mut self) { self.client.borrow_mut().cleanup(); }
}

impl PlayState for SessionState {
    fn enter(&mut self, global_state: &mut GlobalState, _: Direction) {
        // Trap the cursor.
        global_state.window.grab_cursor(true);

        self.client.borrow_mut().clear_terrain();

        // Send startup commands to the server
        if global_state.settings.send_logon_commands {
            for cmd in &global_state.settings.logon_commands {
                self.client.borrow_mut().send_chat(cmd.to_string());
            }
        }
    }

    fn tick(&mut self, global_state: &mut GlobalState, events: Vec<Event>) -> PlayStateResult {
        span!(_guard, "tick", "<Session as PlayState>::tick");
        // NOTE: Not strictly necessary, but useful for hotloading translation changes.
        self.voxygen_i18n = VoxygenLocalization::load_expect(&i18n_asset_key(
            &global_state.settings.language.selected_language,
        ));

        // TODO: can this be a method on the session or are there borrowcheck issues?
        let (client_in_game, client_registered) = {
            let client = self.client.borrow();
            (client.in_game(), client.registered())
        };
        if client_in_game.is_some() {
            // Update MyEntity
            // Note: Alternatively, the client could emit an event when the entity changes
            // which may or may not be more elegant
            {
                let my_entity = self.client.borrow().entity();
                self.client
                    .borrow_mut()
                    .state_mut()
                    .ecs_mut()
                    .insert(MyEntity(my_entity));
            }
            // Compute camera data
            self.scene
                .camera_mut()
                .compute_dependents(&*self.client.borrow().state().terrain());
            let camera::Dependents {
                cam_pos, cam_dir, ..
            } = self.scene.camera().dependents();
            let focus_pos = self.scene.camera().get_focus_pos();
            let focus_off = focus_pos.map(|e| e.trunc());
            let cam_pos = cam_pos + focus_off;

            let (is_aiming, aim_dir_offset) = {
                let client = self.client.borrow();
                let is_aiming = client
                    .state()
                    .read_storage::<comp::CharacterState>()
                    .get(client.entity())
                    .map(|cs| cs.is_aimed())
                    .unwrap_or(false);

                (
                    is_aiming,
                    if is_aiming && self.scene.camera().get_mode() == CameraMode::ThirdPerson {
                        Vec3::unit_z() * 0.05
                    } else {
                        Vec3::zero()
                    },
                )
            };
            self.is_aiming = is_aiming;

            // Check to see whether we're aiming at anything
            let (build_pos, select_pos, target_entity) =
                under_cursor(&self.client.borrow(), cam_pos, cam_dir);
            // Throw out distance info, it will be useful in the future
            self.target_entity = target_entity.map(|x| x.0);

            let can_build = self
                .client
                .borrow()
                .state()
                .read_storage::<comp::CanBuild>()
                .get(self.client.borrow().entity())
                .is_some();

            // Only highlight collectables
            self.scene.set_select_pos(select_pos.filter(|sp| {
                self.client
                    .borrow()
                    .state()
                    .terrain()
                    .get(*sp)
                    .map(|b| b.is_collectible() || can_build)
                    .unwrap_or(false)
            }));

            // Handle window events.
            for event in events {
                // Pass all events to the ui first.
                if self.hud.handle_event(event.clone(), global_state) {
                    continue;
                }

                match event {
                    Event::Close => {
                        return PlayStateResult::Shutdown;
                    },
                    Event::InputUpdate(GameInput::Primary, state) => {
                        // If we can build, use LMB to break blocks, if not, use it to attack
                        let mut client = self.client.borrow_mut();
                        if state && can_build {
                            if let Some(select_pos) = select_pos {
                                client.remove_block(select_pos);
                            }
                        } else {
                            self.inputs.primary.set_state(state);
                        }
                    },

                    Event::InputUpdate(GameInput::Secondary, state) => {
                        self.inputs.secondary.set_state(false); // To be changed later on

                        let mut client = self.client.borrow_mut();

                        if state && can_build {
                            if let Some(build_pos) = build_pos {
                                client.place_block(build_pos, self.selected_block);
                            }
                        } else {
                            self.inputs.secondary.set_state(state);
                        }
                    },

                    Event::InputUpdate(GameInput::Roll, state) => {
                        let client = self.client.borrow();
                        if can_build {
                            if state {
                                if let Some(block) = select_pos
                                    .and_then(|sp| client.state().terrain().get(sp).ok().copied())
                                {
                                    self.selected_block = block;
                                }
                            }
                        } else {
                            self.inputs.roll.set_state(state);
                        }
                    },
                    Event::InputUpdate(GameInput::Respawn, state)
                        if state != self.key_state.respawn =>
                    {
                        self.stop_auto_walk();
                        self.key_state.respawn = state;
                        if state {
                            self.client.borrow_mut().respawn();
                        }
                    }
                    Event::InputUpdate(GameInput::Jump, state) => {
                        self.inputs.jump.set_state(state);
                    },
                    Event::InputUpdate(GameInput::SwimUp, state) => {
                        self.inputs.swimup.set_state(state);
                    },
                    Event::InputUpdate(GameInput::SwimDown, state) => {
                        self.inputs.swimdown.set_state(state);
                    },
                    Event::InputUpdate(GameInput::Sit, state)
                        if state != self.key_state.toggle_sit =>
                    {
                        self.key_state.toggle_sit = state;

                        if state {
                            self.stop_auto_walk();
                            self.client.borrow_mut().toggle_sit();
                        }
                    }
                    Event::InputUpdate(GameInput::Dance, state)
                        if state != self.key_state.toggle_dance =>
                    {
                        self.key_state.toggle_dance = state;
                        if state {
                            self.stop_auto_walk();
                            self.client.borrow_mut().toggle_dance();
                        }
                    }
                    Event::InputUpdate(GameInput::Sneak, state)
                        if state != self.key_state.toggle_sneak =>
                    {
                        self.key_state.toggle_sneak = state;
                        if state {
                            self.stop_auto_walk();
                            self.client.borrow_mut().toggle_sneak();
                        }
                    }
                    Event::InputUpdate(GameInput::MoveForward, state) => {
                        if state && global_state.settings.gameplay.stop_auto_walk_on_input {
                            self.stop_auto_walk();
                        }
                        self.key_state.up = state
                    },
                    Event::InputUpdate(GameInput::MoveBack, state) => {
                        if state && global_state.settings.gameplay.stop_auto_walk_on_input {
                            self.stop_auto_walk();
                        }
                        self.key_state.down = state
                    },
                    Event::InputUpdate(GameInput::MoveLeft, state) => {
                        if state && global_state.settings.gameplay.stop_auto_walk_on_input {
                            self.stop_auto_walk();
                        }
                        self.key_state.left = state
                    },
                    Event::InputUpdate(GameInput::MoveRight, state) => {
                        if state && global_state.settings.gameplay.stop_auto_walk_on_input {
                            self.stop_auto_walk();
                        }
                        self.key_state.right = state
                    },
                    Event::InputUpdate(GameInput::Glide, state)
                        if state != self.key_state.toggle_glide =>
                    {
                        self.key_state.toggle_glide = state;
                        if state {
                            self.client.borrow_mut().toggle_glide();
                        }
                    }
                    Event::InputUpdate(GameInput::Climb, state) => {
                        self.key_state.climb_up = state;
                    },
                    Event::InputUpdate(GameInput::ClimbDown, state) => {
                        self.key_state.climb_down = state;
                    },
                    /*Event::InputUpdate(GameInput::WallLeap, state) => {
                        self.inputs.wall_leap.set_state(state)
                    },*/
                    Event::InputUpdate(GameInput::ToggleWield, state)
                        if state != self.key_state.toggle_wield =>
                    {
                        self.key_state.toggle_wield = state;
                        if state {
                            self.client.borrow_mut().toggle_wield();
                        }
                    }
                    Event::InputUpdate(GameInput::SwapLoadout, state)
                        if state != self.key_state.swap_loadout =>
                    {
                        self.key_state.swap_loadout = state;
                        if state {
                            self.client.borrow_mut().swap_loadout();
                        }
                    }
                    Event::InputUpdate(GameInput::ToggleLantern, true) => {
                        let mut client = self.client.borrow_mut();
                        if client.is_lantern_enabled() {
                            client.disable_lantern();
                        } else {
                            client.enable_lantern();
                        }
                    },
                    Event::InputUpdate(GameInput::Mount, true) => {
                        let mut client = self.client.borrow_mut();
                        if client.is_mounted() {
                            client.unmount();
                        } else {
                            let player_pos = client
                                .state()
                                .read_storage::<comp::Pos>()
                                .get(client.entity())
                                .copied();
                            if let Some(player_pos) = player_pos {
                                // Find closest mountable entity
                                let mut closest_mountable: Option<(specs::Entity, i32)> = None;

                                for (entity, pos, ms) in (
                                    &client.state().ecs().entities(),
                                    &client.state().ecs().read_storage::<comp::Pos>(),
                                    &client.state().ecs().read_storage::<comp::MountState>(),
                                )
                                    .join()
                                    .filter(|(entity, _, _)| *entity != client.entity())
                                {
                                    if comp::MountState::Unmounted != *ms {
                                        continue;
                                    }

                                    let dist =
                                        (player_pos.0.distance_squared(pos.0) * 1000.0) as i32;
                                    if dist > MAX_MOUNT_RANGE_SQR {
                                        continue;
                                    }

                                    if let Some(previous) = closest_mountable.as_mut() {
                                        if dist < previous.1 {
                                            *previous = (entity, dist);
                                        }
                                    } else {
                                        closest_mountable = Some((entity, dist));
                                    }
                                }

                                if let Some((mountee_entity, _)) = closest_mountable {
                                    client.mount(mountee_entity);
                                }
                            }
                        }
                    },
                    Event::InputUpdate(GameInput::Interact, state)
                        if state != self.key_state.collect =>
                    {
                        self.key_state.collect = state;

                        if state {
                            let mut client = self.client.borrow_mut();

                            // Collect terrain sprites
                            if let Some(select_pos) = self.scene.select_pos() {
                                client.collect_block(select_pos);
                            }

                            // Collect lootable entities
                            let player_pos = client
                                .state()
                                .read_storage::<comp::Pos>()
                                .get(client.entity())
                                .copied();

                            if let Some(player_pos) = player_pos {
                                let entity = self.target_entity.or_else(|| {
                                    (
                                        &client.state().ecs().entities(),
                                        &client.state().ecs().read_storage::<comp::Pos>(),
                                        &client.state().ecs().read_storage::<comp::Item>(),
                                    )
                                        .join()
                                        .filter(|(_, pos, _)| {
                                            pos.0.distance_squared(player_pos.0)
                                                < MAX_PICKUP_RANGE_SQR
                                        })
                                        .min_by_key(|(_, pos, _)| {
                                            (pos.0.distance_squared(player_pos.0) * 1000.0) as i32
                                        })
                                        .map(|(entity, _, _)| entity)
                                });

                                if let Some(entity) = entity {
                                    client.pick_up(entity);
                                }
                            }
                        }
                    }
                    /*Event::InputUpdate(GameInput::Charge, state) => {
                        self.inputs.charge.set_state(state);
                    },*/
                    Event::InputUpdate(GameInput::FreeLook, state) => {
                        match (global_state.settings.gameplay.free_look_behavior, state) {
                            (PressBehavior::Toggle, true) => {
                                self.free_look = !self.free_look;
                                self.hud.free_look(self.free_look);
                            },
                            (PressBehavior::Hold, state) => {
                                self.free_look = state;
                                self.hud.free_look(self.free_look);
                            },
                            _ => {},
                        };
                    },
                    Event::InputUpdate(GameInput::AutoWalk, state) => {
                        match (global_state.settings.gameplay.auto_walk_behavior, state) {
                            (PressBehavior::Toggle, true) => {
                                self.auto_walk = !self.auto_walk;
                                self.key_state.auto_walk = self.auto_walk;
                                self.hud.auto_walk(self.auto_walk);
                            },
                            (PressBehavior::Hold, state) => {
                                self.auto_walk = state;
                                self.key_state.auto_walk = self.auto_walk;
                                self.hud.auto_walk(self.auto_walk);
                            },
                            _ => {},
                        }
                    },
                    Event::InputUpdate(GameInput::CycleCamera, true) => {
                        // Prevent accessing camera modes which aren't available in multiplayer
                        // unless you are an admin. This is an easily bypassed clientside check.
                        // The server should do its own filtering of which entities are sent to
                        // clients to prevent abuse.
                        let camera = self.scene.camera_mut();
                        camera.next_mode(self.client.borrow().is_admin());
                    },
                    Event::InputUpdate(GameInput::Select, state) => {
                        if !state {
                            self.selected_entity =
                                self.target_entity.map(|e| (e, std::time::Instant::now()));
                        }
                    },
                    Event::InputUpdate(GameInput::AcceptGroupInvite, true) => {
                        let mut client = self.client.borrow_mut();
                        if client.group_invite().is_some() {
                            client.accept_group_invite();
                        }
                    },
                    Event::InputUpdate(GameInput::DeclineGroupInvite, true) => {
                        let mut client = self.client.borrow_mut();
                        if client.group_invite().is_some() {
                            client.decline_group_invite();
                        }
                    },
                    Event::AnalogGameInput(input) => match input {
                        AnalogGameInput::MovementX(v) => {
                            self.key_state.analog_matrix.x = v;
                        },
                        AnalogGameInput::MovementY(v) => {
                            self.key_state.analog_matrix.y = v;
                        },
                        other => {
                            self.scene.handle_input_event(Event::AnalogGameInput(other));
                        },
                    },
                    Event::ScreenshotMessage(screenshot_message) => {
                        self.hud.new_message(comp::ChatMsg {
                            chat_type: comp::ChatType::CommandInfo,
                            message: screenshot_message,
                        })
                    },

                    // Pass all other events to the scene
                    event => {
                        self.scene.handle_input_event(event);
                    }, // TODO: Do something if the event wasn't handled?
                }
            }

            if !self.free_look {
                self.walk_forward_dir = self.scene.camera().forward_xy();
                self.walk_right_dir = self.scene.camera().right_xy();
                self.inputs.look_dir = Dir::from_unnormalized(cam_dir + aim_dir_offset).unwrap();
            }

            // Get the current state of movement related inputs
            let input_vec = self.key_state.dir_vec();
            let (axis_right, axis_up) = (input_vec[0], input_vec[1]);

            match self.scene.camera().get_mode() {
                camera::CameraMode::FirstPerson | camera::CameraMode::ThirdPerson => {
                    // Move the player character based on their walking direction.
                    // This could be different from the camera direction if free look is enabled.
                    self.inputs.move_dir =
                        self.walk_right_dir * axis_right + self.walk_forward_dir * axis_up;
                    self.freefly_vel = Vec3::zero();
                },

                camera::CameraMode::Freefly => {
                    // Move the camera freely in 3d space. Apply acceleration so that
                    // the movement feels more natural and controlled.
                    const FREEFLY_ACCEL: f32 = 120.0;
                    const FREEFLY_DAMPING: f32 = 80.0;
                    const FREEFLY_MAX_SPEED: f32 = 50.0;

                    let forward = self.scene.camera().forward();
                    let right = self.scene.camera().right();
                    let dir = right * axis_right + forward * axis_up;

                    let dt = global_state.clock.get_last_delta().as_secs_f32();
                    if self.freefly_vel.magnitude_squared() > 0.01 {
                        let new_vel = self.freefly_vel
                            - self.freefly_vel.normalized() * (FREEFLY_DAMPING * dt);
                        if self.freefly_vel.dot(new_vel) > 0.0 {
                            self.freefly_vel = new_vel;
                        } else {
                            self.freefly_vel = Vec3::zero();
                        }
                    }
                    if dir.magnitude_squared() > 0.01 {
                        self.freefly_vel += dir * (FREEFLY_ACCEL * dt);
                        if self.freefly_vel.magnitude() > FREEFLY_MAX_SPEED {
                            self.freefly_vel = self.freefly_vel.normalized() * FREEFLY_MAX_SPEED;
                        }
                    }

                    let pos = self.scene.camera().get_focus_pos();
                    self.scene
                        .camera_mut()
                        .set_focus_pos(pos + self.freefly_vel * dt);

                    // Do not apply any movement to the player character
                    self.inputs.move_dir = Vec2::zero();
                },
            };

            self.inputs.climb = self.key_state.climb();

            let mut outcomes = Vec::new();

            // Runs if either in a multiplayer server or the singleplayer server is unpaused
            if !global_state.paused() {
                // Perform an in-game tick.
                match self.tick(
                    global_state.clock.get_avg_delta(),
                    global_state,
                    &mut outcomes,
                ) {
                    Ok(TickAction::Continue) => {}, // Do nothing
                    Ok(TickAction::Disconnect) => return PlayStateResult::Pop, // Go to main menu
                    Err(err) => {
                        global_state.info_message =
                            Some(self.voxygen_i18n.get("common.connection_lost").to_owned());
                        error!("[session] Failed to tick the scene: {:?}", err);

                        return PlayStateResult::Pop;
                    },
                }
            }

            // Recompute dependents just in case some input modified the camera
            self.scene
                .camera_mut()
                .compute_dependents(&*self.client.borrow().state().terrain());

            // Generate debug info, if needed (it iterates through enough data that we might
            // as well avoid it unless we need it).
            let debug_info = global_state
                .settings
                .gameplay
                .toggle_debug
                .then(|| DebugInfo {
                    tps: global_state.clock.get_tps(),
                    ping_ms: self.client.borrow().get_ping_ms_rolling_avg(),
                    coordinates: self
                        .client
                        .borrow()
                        .state()
                        .ecs()
                        .read_storage::<Pos>()
                        .get(self.client.borrow().entity())
                        .cloned(),
                    velocity: self
                        .client
                        .borrow()
                        .state()
                        .ecs()
                        .read_storage::<Vel>()
                        .get(self.client.borrow().entity())
                        .cloned(),
                    ori: self
                        .client
                        .borrow()
                        .state()
                        .ecs()
                        .read_storage::<comp::Ori>()
                        .get(self.client.borrow().entity())
                        .cloned(),
                    num_chunks: self.scene.terrain().chunk_count() as u32,
                    num_lights: self.scene.lights().len() as u32,
                    num_visible_chunks: self.scene.terrain().visible_chunk_count() as u32,
                    num_shadow_chunks: self.scene.terrain().shadow_chunk_count() as u32,
                    num_figures: self.scene.figure_mgr().figure_count() as u32,
                    num_figures_visible: self.scene.figure_mgr().figure_count_visible() as u32,
                    num_particles: self.scene.particle_mgr().particle_count() as u32,
                    num_particles_visible: self.scene.particle_mgr().particle_count_visible()
                        as u32,
                });

            // Extract HUD events ensuring the client borrow gets dropped.
            let mut hud_events = self.hud.maintain(
                &self.client.borrow(),
                global_state,
                &debug_info,
                &self.scene.camera(),
                global_state.clock.get_last_delta(),
                HudInfo {
                    is_aiming,
                    is_first_person: matches!(
                        self.scene.camera().get_mode(),
                        camera::CameraMode::FirstPerson
                    ),
                    target_entity: self.target_entity,
                    selected_entity: self.selected_entity,
                },
            );

            // Look for changes in the localization files
            if global_state.localization_watcher.reloaded() {
                hud_events.push(HudEvent::ChangeLanguage(Box::new(
                    self.voxygen_i18n.metadata.clone(),
                )));
            }

            // Maintain the UI.
            for event in hud_events {
                match event {
                    HudEvent::SendMessage(msg) => {
                        // TODO: Handle result
                        self.client.borrow_mut().send_chat(msg);
                    },
                    HudEvent::CharacterSelection => {
                        self.client.borrow_mut().request_remove_character()
                    },
                    HudEvent::Logout => self.client.borrow_mut().request_logout(),
                    HudEvent::Quit => {
                        return PlayStateResult::Shutdown;
                    },
                    HudEvent::AdjustMousePan(sensitivity) => {
                        global_state.window.pan_sensitivity = sensitivity;
                        global_state.settings.gameplay.pan_sensitivity = sensitivity;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::AdjustMouseZoom(sensitivity) => {
                        global_state.window.zoom_sensitivity = sensitivity;
                        global_state.settings.gameplay.zoom_sensitivity = sensitivity;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ToggleZoomInvert(zoom_inverted) => {
                        global_state.window.zoom_inversion = zoom_inverted;
                        global_state.settings.gameplay.zoom_inversion = zoom_inverted;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::Sct(sct) => {
                        global_state.settings.gameplay.sct = sct;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::SctPlayerBatch(sct_player_batch) => {
                        global_state.settings.gameplay.sct_player_batch = sct_player_batch;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ToggleTips(loading_tips) => {
                        global_state.settings.gameplay.loading_tips = loading_tips;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::SctDamageBatch(sct_damage_batch) => {
                        global_state.settings.gameplay.sct_damage_batch = sct_damage_batch;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::SpeechBubbleDarkMode(sbdm) => {
                        global_state.settings.gameplay.speech_bubble_dark_mode = sbdm;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::SpeechBubbleIcon(sbi) => {
                        global_state.settings.gameplay.speech_bubble_icon = sbi;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ToggleDebug(toggle_debug) => {
                        global_state.settings.gameplay.toggle_debug = toggle_debug;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ToggleMouseYInvert(mouse_y_inverted) => {
                        global_state.window.mouse_y_inversion = mouse_y_inverted;
                        global_state.settings.gameplay.mouse_y_inversion = mouse_y_inverted;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ToggleSmoothPan(smooth_pan_enabled) => {
                        global_state.settings.gameplay.smooth_pan_enable = smooth_pan_enabled;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::AdjustViewDistance(view_distance) => {
                        self.client.borrow_mut().set_view_distance(view_distance);

                        global_state.settings.graphics.view_distance = view_distance;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::AdjustLodDetail(lod_detail) => {
                        self.scene.lod.set_detail(lod_detail);

                        global_state.settings.graphics.lod_detail = lod_detail;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::AdjustSpriteRenderDistance(sprite_render_distance) => {
                        global_state.settings.graphics.sprite_render_distance =
                            sprite_render_distance;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::AdjustFigureLoDRenderDistance(figure_lod_render_distance) => {
                        global_state.settings.graphics.figure_lod_render_distance =
                            figure_lod_render_distance;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::CrosshairTransp(crosshair_transp) => {
                        global_state.settings.gameplay.crosshair_transp = crosshair_transp;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ChatTransp(chat_transp) => {
                        global_state.settings.gameplay.chat_transp = chat_transp;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ChatCharName(chat_char_name) => {
                        global_state.settings.gameplay.chat_character_name = chat_char_name;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::CrosshairType(crosshair_type) => {
                        global_state.settings.gameplay.crosshair_type = crosshair_type;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::Intro(intro_show) => {
                        global_state.settings.gameplay.intro_show = intro_show;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ToggleXpBar(xp_bar) => {
                        global_state.settings.gameplay.xp_bar = xp_bar;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ToggleBarNumbers(bar_numbers) => {
                        global_state.settings.gameplay.bar_numbers = bar_numbers;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ToggleShortcutNumbers(shortcut_numbers) => {
                        global_state.settings.gameplay.shortcut_numbers = shortcut_numbers;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::UiScale(scale_change) => {
                        global_state.settings.gameplay.ui_scale =
                            self.hud.scale_change(scale_change);
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::AdjustMusicVolume(music_volume) => {
                        global_state.audio.set_music_volume(music_volume);

                        global_state.settings.audio.music_volume = music_volume;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::AdjustSfxVolume(sfx_volume) => {
                        global_state.audio.set_sfx_volume(sfx_volume);

                        global_state.settings.audio.sfx_volume = sfx_volume;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ChangeAudioDevice(name) => {
                        global_state.audio.set_device(name.clone());

                        global_state.settings.audio.output = AudioOutput::Device(name);
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ChangeMaxFPS(fps) => {
                        global_state.settings.graphics.max_fps = fps;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::UseSlot(x) => self.client.borrow_mut().use_slot(x),
                    HudEvent::SwapSlots(a, b) => self.client.borrow_mut().swap_slots(a, b),
                    HudEvent::DropSlot(x) => {
                        let mut client = self.client.borrow_mut();
                        client.drop_slot(x);
                        if let comp::slot::Slot::Equip(equip_slot) = x {
                            if let comp::slot::EquipSlot::Lantern = equip_slot {
                                client.disable_lantern();
                            }
                        }
                    },
                    HudEvent::ChangeHotbarState(state) => {
                        let client = self.client.borrow();

                        let server = &client.server_info.name;
                        // If we are changing the hotbar state this CANNOT be None.
                        let character_id = client.active_character_id.unwrap();

                        // Get or update the ServerProfile.
                        global_state
                            .profile
                            .set_hotbar_slots(server, character_id, state.slots);

                        global_state.profile.save_to_file_warn();

                        info!("Event! -> ChangedHotbarState")
                    },
                    HudEvent::Ability3(state) => self.inputs.ability3.set_state(state),
                    HudEvent::ChangeFOV(new_fov) => {
                        global_state.settings.graphics.fov = new_fov;
                        global_state.settings.save_to_file_warn();
                        self.scene.camera_mut().set_fov_deg(new_fov);
                        self.scene
                            .camera_mut()
                            .compute_dependents(&*self.client.borrow().state().terrain());
                    },
                    HudEvent::MapZoom(map_zoom) => {
                        global_state.settings.gameplay.map_zoom = map_zoom;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ChangeGamma(new_gamma) => {
                        global_state.settings.graphics.gamma = new_gamma;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ChangeAmbiance(new_ambiance) => {
                        global_state.settings.graphics.ambiance = new_ambiance;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ChangeRenderMode(new_render_mode) => {
                        // Do this first so if it crashes the setting isn't saved :)
                        global_state
                            .window
                            .renderer_mut()
                            .set_render_mode((&*new_render_mode).clone())
                            .unwrap();
                        global_state.settings.graphics.render_mode = *new_render_mode;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ChangeLanguage(new_language) => {
                        global_state.settings.language.selected_language =
                            new_language.language_identifier;
                        self.voxygen_i18n = VoxygenLocalization::load_watched(
                            &i18n_asset_key(&global_state.settings.language.selected_language),
                            &mut global_state.localization_watcher,
                        )
                        .unwrap();
                        self.voxygen_i18n.log_missing_entries();
                        self.hud.update_language(Arc::clone(&self.voxygen_i18n));
                    },
                    HudEvent::ChangeFullscreenMode(new_fullscreen_settings) => {
                        global_state
                            .window
                            .set_fullscreen_mode(new_fullscreen_settings);
                        global_state.settings.graphics.fullscreen = new_fullscreen_settings;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ToggleParticlesEnabled(particles_enabled) => {
                        global_state.settings.graphics.particles_enabled = particles_enabled;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::AdjustWindowSize(new_size) => {
                        global_state.window.set_size(new_size.into());
                        global_state.settings.graphics.window_size = new_size;
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ChangeBinding(game_input) => {
                        global_state.window.set_keybinding_mode(game_input);
                    },
                    HudEvent::ResetBindings => {
                        global_state.settings.controls = ControlSettings::default();
                        global_state.settings.save_to_file_warn();
                    },
                    HudEvent::ChangeFreeLookBehavior(behavior) => {
                        global_state.settings.gameplay.free_look_behavior = behavior;
                    },
                    HudEvent::ChangeAutoWalkBehavior(behavior) => {
                        global_state.settings.gameplay.auto_walk_behavior = behavior;
                    },
                    HudEvent::ChangeStopAutoWalkOnInput(state) => {
                        global_state.settings.gameplay.stop_auto_walk_on_input = state;
                    },
                    HudEvent::CraftRecipe(r) => {
                        self.client.borrow_mut().craft_recipe(&r);
                    },
                    HudEvent::InviteMember(uid) => {
                        self.client.borrow_mut().send_group_invite(uid);
                    },
                    HudEvent::AcceptInvite => {
                        self.client.borrow_mut().accept_group_invite();
                    },
                    HudEvent::DeclineInvite => {
                        self.client.borrow_mut().decline_group_invite();
                    },
                    HudEvent::KickMember(uid) => {
                        self.client.borrow_mut().kick_from_group(uid);
                    },
                    HudEvent::LeaveGroup => {
                        self.client.borrow_mut().leave_group();
                    },
                    HudEvent::AssignLeader(uid) => {
                        self.client.borrow_mut().assign_group_leader(uid);
                    },
                }
            }

            {
                let client = self.client.borrow();
                let scene_data = SceneData {
                    state: client.state(),
                    player_entity: client.entity(),
                    target_entity: self.target_entity,
                    loaded_distance: client.loaded_distance(),
                    view_distance: client.view_distance().unwrap_or(1),
                    tick: client.get_tick(),
                    thread_pool: client.thread_pool(),
                    gamma: global_state.settings.graphics.gamma,
                    ambiance: global_state.settings.graphics.ambiance,
                    mouse_smoothing: global_state.settings.gameplay.smooth_pan_enable,
                    sprite_render_distance: global_state.settings.graphics.sprite_render_distance
                        as f32,
                    particles_enabled: global_state.settings.graphics.particles_enabled,
                    figure_lod_render_distance: global_state
                        .settings
                        .graphics
                        .figure_lod_render_distance
                        as f32,
                    is_aiming,
                };

                // Runs if either in a multiplayer server or the singleplayer server is unpaused
                if !global_state.paused() {
                    self.scene.maintain(
                        global_state.window.renderer_mut(),
                        &mut global_state.audio,
                        &scene_data,
                    );

                    // Process outcomes from client
                    for outcome in outcomes {
                        self.scene
                            .handle_outcome(&outcome, &scene_data, &mut global_state.audio);
                    }
                }
            }

            // Clean things up after the tick.
            self.cleanup();

            PlayStateResult::Continue
        } else if client_registered && client_in_game.is_none() {
            PlayStateResult::Switch(Box::new(CharSelectionState::new(
                global_state,
                Rc::clone(&self.client),
            )))
        } else {
            error!("Client not in the expected state, exiting session play state");
            PlayStateResult::Pop
        }
    }

    fn name(&self) -> &'static str { "Session" }

    /// Render the session to the screen.
    ///
    /// This method should be called once per frame.
    fn render(&mut self, renderer: &mut Renderer, settings: &Settings) {
        span!(_guard, "render", "<Session as PlayState>::render");
        // Render the screen using the global renderer
        {
            let client = self.client.borrow();

            let scene_data = SceneData {
                state: client.state(),
                player_entity: client.entity(),
                target_entity: self.target_entity,
                loaded_distance: client.loaded_distance(),
                view_distance: client.view_distance().unwrap_or(1),
                tick: client.get_tick(),
                thread_pool: client.thread_pool(),
                gamma: settings.graphics.gamma,
                ambiance: settings.graphics.ambiance,
                mouse_smoothing: settings.gameplay.smooth_pan_enable,
                sprite_render_distance: settings.graphics.sprite_render_distance as f32,
                figure_lod_render_distance: settings.graphics.figure_lod_render_distance as f32,
                particles_enabled: settings.graphics.particles_enabled,
                is_aiming: self.is_aiming,
            };
            self.scene.render(
                renderer,
                client.state(),
                client.entity(),
                client.get_tick(),
                &scene_data,
            );
        }
        // Draw the UI to the screen
        self.hud.render(renderer, self.scene.globals());
    }
}

/// Max distance an entity can be "targeted"
const MAX_TARGET_RANGE: f32 = 300.0;
/// Calculate what the cursor is pointing at within the 3d scene
#[allow(clippy::type_complexity)]
fn under_cursor(
    client: &Client,
    cam_pos: Vec3<f32>,
    cam_dir: Vec3<f32>,
) -> (
    Option<Vec3<i32>>,
    Option<Vec3<i32>>,
    Option<(specs::Entity, f32)>,
) {
    // Choose a spot above the player's head for item distance checks
    let player_entity = client.entity();
    let player_pos = match client
        .state()
        .read_storage::<comp::Pos>()
        .get(player_entity)
    {
        Some(pos) => pos.0 + (Vec3::unit_z() * 2.0),
        _ => cam_pos, // Should never happen, but a safe fallback
    };
    let terrain = client.state().terrain();

    let cam_ray = terrain
        .ray(cam_pos, cam_pos + cam_dir * 100.0)
        .until(|block| block.is_filled() || block.is_collectible())
        .cast();

    let cam_dist = cam_ray.0;

    // The ray hit something, is it within range?
    let (build_pos, select_pos) = if matches!(cam_ray.1, Ok(Some(_)) if
        player_pos.distance_squared(cam_pos + cam_dir * cam_dist)
        <= MAX_PICKUP_RANGE_SQR)
    {
        (
            Some((cam_pos + cam_dir * (cam_dist - 0.01)).map(|e| e.floor() as i32)),
            Some((cam_pos + cam_dir * (cam_dist + 0.01)).map(|e| e.floor() as i32)),
        )
    } else {
        (None, None)
    };

    // See if ray hits entities
    // Currently treated as spheres
    let ecs = client.state().ecs();
    // Don't cast through blocks
    // Could check for intersection with entity from last frame to narrow this down
    let cast_dist = if let Ok(Some(_)) = cam_ray.1 {
        cam_dist.min(MAX_TARGET_RANGE)
    } else {
        MAX_TARGET_RANGE
    };

    // Need to raycast by distance to cam
    // But also filter out by distance to the player (but this only needs to be done
    // on final result)
    let mut nearby = (
        &ecs.entities(),
        &ecs.read_storage::<comp::Pos>(),
        ecs.read_storage::<comp::Scale>().maybe(),
        &ecs.read_storage::<comp::Body>()
    )
        .join()
        .filter(|(e, _, _, _)| *e != player_entity)
        .map(|(e, p, s, b)| {
            const RADIUS_SCALE: f32 = 3.0;
            let radius = s.map_or(1.0, |s| s.0) * b.radius() * RADIUS_SCALE;
            // Move position up from the feet
            let pos = Vec3::new(p.0.x, p.0.y, p.0.z + radius);
            // Distance squared from camera to the entity
            let dist_sqr = pos.distance_squared(cam_pos);
            (e, pos, radius, dist_sqr)
        })
        // Roughly filter out entities farther than ray distance
        .filter(|(_, _, r, d_sqr)| *d_sqr <= cast_dist.powi(2) + 2.0 * cast_dist * r + r.powi(2))
        // Ignore entities intersecting the camera
        .filter(|(_, _, r, d_sqr)| *d_sqr > r.powi(2))
        // Substract sphere radius from distance to the camera
        .map(|(e, p, r, d_sqr)| (e, p, r, d_sqr.sqrt() - r))
        .collect::<Vec<_>>();
    // Sort by distance
    nearby.sort_unstable_by(|a, b| a.3.partial_cmp(&b.3).unwrap());

    let seg_ray = LineSegment3 {
        start: cam_pos,
        end: cam_pos + cam_dir * cam_dist,
    };
    // TODO: fuzzy borders
    let target_entity = nearby
        .iter()
        .map(|(e, p, r, _)| (e, *p, r))
        // Find first one that intersects the ray segment
        .find(|(_, p, r)| seg_ray.projected_point(*p).distance_squared(*p) < r.powi(2))
        .and_then(|(e, p, r)| {
            let dist_to_player = p.distance(player_pos);
            (dist_to_player - r < MAX_TARGET_RANGE).then_some((*e, dist_to_player))
        });

    // TODO: consider setting build/select to None when targeting an entity
    (build_pos, select_pos, target_entity)
}
