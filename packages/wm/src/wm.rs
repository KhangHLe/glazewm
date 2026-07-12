use anyhow::{bail, Context};
use tokio::sync::mpsc::{self};
use tracing::warn;
use uuid::Uuid;
#[cfg(target_os = "windows")]
use wm_common::TitleBarVisibility;
use wm_common::{
  ContainerDto, FloatingStateConfig, FullscreenStateConfig,
  InvokeCommand, TilingDirection, WindowState, WmEvent,
};
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;
#[cfg(target_os = "windows")]
use crate::commands::window::detach_window_for_close;
use wm_platform::{
  Dispatcher, LengthValue, PlatformEvent, RectDelta, WindowEvent,
};
#[cfg(target_os = "windows")]
use wm_platform::OpacityValue;

use crate::{
  commands::{
    container::{
      focus_container_by_id, focus_in_direction, set_tiling_direction,
      toggle_tiling_direction,
    },
    general::{
      cycle_focus, disable_binding_mode, enable_binding_mode,
      platform_sync, reload_config, shell_exec, toggle_pause,
    },
    monitor::focus_monitor,
    window::{
      ignore_window, move_window_in_direction, move_window_to_workspace,
      resize_window, set_window_position, set_window_size,
      update_window_state, WindowPositionTarget,
    },
    workspace::{
      focus_workspace, move_workspace_in_direction,
      update_workspace_config,
    },
  },
  events::{
    handle_display_settings_changed, handle_mouse_move,
    handle_window_destroyed, handle_window_focused, handle_window_hidden,
    handle_window_minimize_ended, handle_window_minimized,
    handle_window_moved_or_resized, handle_window_shown,
    handle_window_title_changed,
  },
  ipc_server::IpcServer,
  models::{Container, WorkspaceTarget},
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub struct WindowManager {
  pub event_rx: mpsc::UnboundedReceiver<WmEvent>,
  pub exit_rx: mpsc::UnboundedReceiver<()>,
  pub animation_tick_rx: mpsc::UnboundedReceiver<()>,
  pub state: WmState,
}

impl WindowManager {
  pub fn new(
    config: &mut UserConfig,
    dispatcher: Dispatcher,
  ) -> anyhow::Result<Self> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (exit_tx, exit_rx) = mpsc::unbounded_channel();
    let (animation_tick_tx, animation_tick_rx) = mpsc::unbounded_channel();

    let mut state = WmState::new(
      dispatcher,
      event_tx,
      exit_tx,
      animation_tick_tx,
    );
    state.populate(config)?;

    // Start animation timer if `populate` created any animations. This
    // mirrors the `ensure_timer_running` call at the end of `process_event`
    // for the initial population path.
    state.animation_manager.ensure_timer_running();

    Ok(Self {
      event_rx,
      exit_rx,
      animation_tick_rx,
      state,
    })
  }

  pub fn process_event(
    &mut self,
    event: PlatformEvent,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    let state = &mut self.state;

    match event {
      PlatformEvent::DisplaySettingsChanged => {
        handle_display_settings_changed(state, config)
      }
      PlatformEvent::Keybinding(keybinding_event) => {
        // Find the keybinding config that matches this keybinding.
        let commands = config
          .active_keybinding_configs(
            &self.state.binding_modes,
            self.state.is_paused,
          )
          .find(|kb_config| {
            kb_config.bindings.contains(&keybinding_event.0)
          })
          .map(|kb_config| kb_config.commands.clone());

        if let Some(commands) = commands {
          self.process_commands(&commands, None, config)?;
        }

        // Return early since we don't want to redraw twice.
        return Ok(());
      }
      PlatformEvent::Mouse(event) => {
        handle_mouse_move(&event, state, config)
      }
      PlatformEvent::Window(window_event) => match window_event {
        WindowEvent::Focused { window, .. } => {
          handle_window_focused(&window, state, config)
        }
        WindowEvent::Shown { window, .. } => {
          handle_window_shown(window, state, config)
        }
        WindowEvent::Hidden { window, .. } => {
          handle_window_hidden(&window, state, config)
        }
        WindowEvent::MovedOrResized {
          window,
          is_interactive_start,
          is_interactive_end,
          ..
        } => handle_window_moved_or_resized(
          &window,
          is_interactive_start,
          is_interactive_end,
          state,
          config,
        ),
        WindowEvent::Minimized { window, .. } => {
          handle_window_minimized(&window, state, config)
        }
        WindowEvent::MinimizeEnded { window, .. } => {
          handle_window_minimize_ended(&window, state, config)
        }
        WindowEvent::TitleChanged { window, .. } => {
          handle_window_title_changed(&window, state, config)
        }
        WindowEvent::Destroyed { window_id, .. } => {
          handle_window_destroyed(window_id, state)
        }
      },
    }?;

    if !state.is_paused && state.pending_sync.has_changes() {
      platform_sync(state, config)?;
    }

    self.state.animation_manager.ensure_timer_running();

    Ok(())
  }

  /// Processes a hook-driven window drag lifecycle event
  /// (grab-and-move).
  ///
  /// Runs the same handler a native interactive move produces, so
  /// `active_drag` and tiling reflow behave identically to a title-bar
  /// drag.
  pub fn process_window_drag(
    &mut self,
    native_window: &wm_platform::NativeWindow,
    is_start: bool,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    let state = &mut self.state;

    handle_window_moved_or_resized(
      native_window,
      is_start,
      !is_start,
      state,
      config,
    )?;

    if !state.is_paused && state.pending_sync.has_changes() {
      platform_sync(state, config)?;
    }

    Ok(())
  }

  /// Updates all active animations and redraws windows that are animating.
  pub fn update_animations(
    &mut self,
    config: &UserConfig,
  ) -> anyhow::Result<()> {
    use crate::animation::AnimationManager;
    // Access animation_manager through state to avoid double borrow
    AnimationManager::update_internal(&mut self.state, config)
  }

  pub fn process_commands(
    &mut self,
    commands: &Vec<InvokeCommand>,
    subject_container_id: Option<Uuid>,
    config: &mut UserConfig,
  ) -> anyhow::Result<Uuid> {
    let state = &mut self.state;

    // Get the container to run WM commands with.
    let subject_container = match subject_container_id {
      Some(id) => state.container_by_id(id).with_context(|| {
        format!("No container found with the given ID '{id}'.")
      })?,
      None => state
        .focused_container()
        .context("No subject container for command.")?,
    };

    let new_subject_container_id = WindowManager::run_commands(
      commands,
      subject_container,
      state,
      config,
    )?;

    if state.pending_sync.has_changes() {
      platform_sync(state, config)?;
    }

    // Start animation timer if animations were created by a command (e.g.
    // startup commands or IPC commands). Without this, surrogate animations
    // started outside of the platform event loop would never tick, leaving
    // windows permanently cloaked.
    self.state.animation_manager.ensure_timer_running();

    Ok(new_subject_container_id)
  }

  pub fn process_wm_event(
    &mut self,
    event: &WmEvent,
    config: &UserConfig,
  ) -> anyhow::Result<()> {
    if !config.value.general.auto_set_tiling_direction {
      return Ok(());
    }

    let Some(window_id) = auto_tiling_window_id(event) else {
      return Ok(());
    };

    let Some(container) = self.state.container_by_id(window_id) else {
      return Ok(());
    };

    let Ok(window) = container.as_window_container() else {
      return Ok(());
    };

    if window.state() != WindowState::Tiling {
      return Ok(());
    }

    let rect = window.to_rect()?;
    let Some(tiling_direction) =
      auto_tiling_direction(rect.width(), rect.height())
    else {
      return Ok(());
    };

    set_tiling_direction(
      window.into(),
      &mut self.state,
      config,
      &tiling_direction,
    )
  }

  pub fn run_commands(
    commands: &Vec<InvokeCommand>,
    subject_container: Container,
    state: &mut WmState,
    config: &mut UserConfig,
  ) -> anyhow::Result<Uuid> {
    let mut current_subject_container = subject_container;

    for command in commands {
      WindowManager::run_command(
        command,
        current_subject_container.clone(),
        state,
        config,
      )?;

      // Update the subject container in case the container type changes.
      // For example, when going from a tiling to a floating window.
      current_subject_container =
        if current_subject_container.is_detached() {
          match state.container_by_id(current_subject_container.id()) {
            Some(container) => container,
            None => break,
          }
        } else {
          current_subject_container
        }
    }

    Ok(current_subject_container.id())
  }

  #[allow(clippy::too_many_lines)]
  pub fn run_command(
    command: &InvokeCommand,
    subject_container: Container,
    state: &mut WmState,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    // No-op if WM is currently paused.
    if state.is_paused && *command != InvokeCommand::WmTogglePause {
      return Ok(());
    }

    if subject_container.is_detached() {
      bail!("Cannot run command because subject container is detached.");
    }

    match &command {
      InvokeCommand::AdjustBorders(args) => {
        match subject_container.as_window_container() {
          Ok(window) => {
            let args = args.clone();
            let border_delta = RectDelta::new(
              args.left.unwrap_or(LengthValue::from_px(0)),
              args.top.unwrap_or(LengthValue::from_px(0)),
              args.right.unwrap_or(LengthValue::from_px(0)),
              args.bottom.unwrap_or(LengthValue::from_px(0)),
            );

            window.set_border_delta(border_delta);
            state.pending_sync.queue_container_to_redraw(window);

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::Close => {
        match subject_container.as_window_container() {
          Ok(window) => {
            #[cfg(target_os = "windows")]
            if config.value.animations.window_close.enabled {
              use wm_platform::NativeWindowWindowsExt;

              let effect_cfg = if window.id()
                == state
                  .focused_container()
                  .map(|c| c.id())
                  .unwrap_or_default()
              {
                &config.value.window_effects.focused_window
              } else {
                &config.value.window_effects.other_windows
              };
              let effect_opacity = if effect_cfg.transparency.enabled {
                effect_cfg.transparency.opacity.to_alpha()
              } else {
                u8::MAX
              };
              let corner_style = if effect_cfg.corner_style.enabled {
                effect_cfg.corner_style.style.clone()
              } else {
                wm_platform::CornerStyle::Default
              };

              if let Ok(rect) = window.to_rect().and_then(|r| {
                window.total_border_delta().map(|d| r.apply_delta(&d, None))
              }) {
                let window_id = window.id();

                // Create and show the surrogate over the still-visible window
                // first. The surrogate captures the live window as a
                // pixel-identical overlay, so this is invisible to the user.
                {
                  let native_ref = window.native();
                  state.animation_manager.start_close_animation(
                    window_id,
                    rect,
                    effect_opacity,
                    corner_style,
                    config,
                    &*native_ref,
                  );
                }

                // If the surrogate was created successfully, cloak the real
                // window — now that the surrogate is up and covering it — and
                // detach it from the layout tree so sibling windows begin
                // their reflow animations in parallel with the close
                // surrogate. Cloaking *after* the surrogate is shown avoids a
                // one-frame gap where the slow `IApplicationView` cloak has
                // hidden the window but the surrogate has not yet been
                // composited, which briefly exposes the desktop.
                // `AnimationManager::update_internal` sends `WM_CLOSE` once the
                // close animation finishes.
                if state
                  .animation_manager
                  .has_close_animation(&window_id)
                {
                  let _ = window.native().set_cloaked(true);
                  detach_window_for_close(window, state)?;
                  return Ok(());
                }
              }
            }

            // Fallback: animations disabled, rect unavailable, or surrogate
            // creation failed — close immediately.
            if let Err(err) = window.native().close() {
              warn!("Failed to close window: {:?}", err);
            }

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::Focus(args) => {
        if let Some(direction) = &args.direction {
          focus_in_direction(&subject_container, direction, state)?;
        }

        if let Some(direction) = &args.workspace_in_direction {
          focus_workspace(
            WorkspaceTarget::Direction(direction.clone()),
            state,
            config,
          )?;
        }

        if let Some(container_id) = &args.container_id {
          focus_container_by_id(container_id, state)?;
        }

        if let Some(name) = &args.workspace {
          focus_workspace(
            WorkspaceTarget::Name(name.clone()),
            state,
            config,
          )?;
        }

        if let Some(name) = &args.workspace_on_monitor {
          focus_workspace(
            WorkspaceTarget::NameOnMonitor(name.clone()),
            state,
            config,
          )?;
        }

        if let Some(monitor_index) = &args.monitor {
          focus_monitor(*monitor_index, state, config)?;
        }

        if args.next_active_workspace {
          focus_workspace(WorkspaceTarget::NextActive, state, config)?;
        }

        if args.prev_active_workspace {
          focus_workspace(WorkspaceTarget::PreviousActive, state, config)?;
        }

        if args.next_workspace {
          focus_workspace(WorkspaceTarget::Next, state, config)?;
        }

        if args.prev_workspace {
          focus_workspace(WorkspaceTarget::Previous, state, config)?;
        }

        if args.recent_workspace {
          focus_workspace(WorkspaceTarget::Recent, state, config)?;
        }

        if args.next_active_workspace_on_monitor {
          focus_workspace(
            WorkspaceTarget::NextActiveInMonitor,
            state,
            config,
          )?;
        }

        if args.prev_active_workspace_on_monitor {
          focus_workspace(
            WorkspaceTarget::PreviousActiveInMonitor,
            state,
            config,
          )?;
        }

        Ok(())
      }
      InvokeCommand::Ignore => {
        match subject_container.as_window_container() {
          Ok(window) => ignore_window(window, state),
          _ => Ok(()),
        }
      }
      InvokeCommand::Move(args) => {
        match subject_container.as_window_container() {
          Ok(window) => {
            if let Some(direction) = &args.direction {
              move_window_in_direction(
                window.clone(),
                direction,
                state,
                config,
              )?;
            }

            if let Some(direction) = &args.workspace_in_direction {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Direction(direction.clone()),
                state,
                config,
              )?;
            }

            if let Some(name) = &args.workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Name(name.clone()),
                state,
                config,
              )?;
            }

            if let Some(name) = &args.workspace_on_monitor {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::NameOnMonitor(name.clone()),
                state,
                config,
              )?;
            }

            if args.next_active_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::NextActive,
                state,
                config,
              )?;
            }

            if args.prev_active_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::PreviousActive,
                state,
                config,
              )?;
            }

            if args.next_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Next,
                state,
                config,
              )?;
            }

            if args.prev_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Previous,
                state,
                config,
              )?;
            }

            if args.recent_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Recent,
                state,
                config,
              )?;
            }

            if args.next_active_workspace_on_monitor {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::NextActiveInMonitor,
                state,
                config,
              )?;
            }

            if args.prev_active_workspace_on_monitor {
              move_window_to_workspace(
                window,
                WorkspaceTarget::PreviousActiveInMonitor,
                state,
                config,
              )?;
            }
            Ok(())
          }

          _ => Ok(()),
        }
      }
      InvokeCommand::MoveWorkspace { direction } => {
        let workspace =
          subject_container.workspace().context("No workspace.")?;

        move_workspace_in_direction(&workspace, direction, state, config)
      }
      InvokeCommand::Position(args) => {
        match subject_container.as_window_container() {
          Ok(window) => {
            if args.centered {
              set_window_position(
                window,
                &WindowPositionTarget::Centered,
                state,
              )
            } else {
              set_window_position(
                window,
                &WindowPositionTarget::Coordinates(args.x_pos, args.y_pos),
                state,
              )
            }
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::UpdateWorkspaceConfig {
        workspace,
        new_config,
      } => {
        let workspace = if let Some(workspace_name) = workspace {
          state
            .workspace_by_name(workspace_name)
            .context("Workspace doesn't exist.")?
        } else {
          subject_container.workspace().context("No workspace.")?
        };
        update_workspace_config(&workspace, state, config, new_config)
      }
      InvokeCommand::Resize(args) => {
        match subject_container.as_window_container() {
          Ok(window) => resize_window(
            &window,
            args.width.clone(),
            args.height.clone(),
            state,
          ),
          _ => Ok(()),
        }
      }
      InvokeCommand::SetFloating {
        centered,
        shown_on_top,
        x_pos,
        y_pos,
        width,
        height,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let floating_defaults =
            &config.value.window_behavior.state_defaults.floating;
          let centered = centered.unwrap_or(floating_defaults.centered);

          let window = update_window_state(
            window.clone(),
            WindowState::Floating(FloatingStateConfig {
              centered,
              shown_on_top: shown_on_top
                .unwrap_or(floating_defaults.shown_on_top),
              transparency: floating_defaults.transparency.clone(),
            }),
            state,
            config,
          )?;

          // Allow size and position to be set if window has not previously
          // been manually placed.
          if !window.has_custom_floating_placement() {
            if width.is_some() || height.is_some() {
              set_window_size(
                window.clone(),
                width.clone(),
                height.clone(),
                state,
              )?;
            }

            if centered {
              set_window_position(
                window,
                &WindowPositionTarget::Centered,
                state,
              )?;
            } else if x_pos.is_some() || y_pos.is_some() {
              set_window_position(
                window,
                &WindowPositionTarget::Coordinates(*x_pos, *y_pos),
                state,
              )?;
            }
          }

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::SetFullscreen {
        maximized,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let fullscreen_defaults =
            &config.value.window_behavior.state_defaults.fullscreen;

          update_window_state(
            window.clone(),
            WindowState::Fullscreen(FullscreenStateConfig {
              maximized: maximized
                .unwrap_or(fullscreen_defaults.maximized),
              shown_on_top: shown_on_top
                .unwrap_or(fullscreen_defaults.shown_on_top),
              transparency: fullscreen_defaults.transparency.clone(),
            }),
            state,
            config,
          )?;

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::SetMinimized => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window.clone(),
              WindowState::Minimized,
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::SetTiling => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window,
              WindowState::Tiling,
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::SetTitleBarVisibility {
        // LINT: `visibility` is only used on Windows.
        #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
        visibility,
      } => match subject_container.as_window_container() {
        #[cfg(target_os = "windows")]
        Ok(window) => {
          _ = window.native().set_title_bar_visibility(
            *visibility == TitleBarVisibility::Shown,
          );
          Ok(())
        }
        _ => Ok(()),
      },
      // LINT: `args` is only used on Windows.
      #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
      InvokeCommand::SetTransparency(args) => {
        match subject_container.as_window_container() {
          #[cfg(target_os = "windows")]
          Ok(window) => {
            if let Some(opacity) = &args.opacity {
              _ = window.native().set_transparency(opacity);
            }

            if let Some(opacity_delta) = &args.opacity_delta {
              _ = window.native().adjust_transparency(opacity_delta);
            }

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ShellExec {
        hide_window,
        command,
      } => shell_exec(&command.join(" "), *hide_window, state),
      InvokeCommand::Size(args) => {
        match subject_container.as_window_container() {
          Ok(window) => set_window_size(
            window,
            args.width.clone(),
            args.height.clone(),
            state,
          ),
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleFloating {
        centered,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let floating_defaults =
            &config.value.window_behavior.state_defaults.floating;

          let centered = centered.unwrap_or(floating_defaults.centered);
          let target_state = WindowState::Floating(FloatingStateConfig {
            centered,
            shown_on_top: shown_on_top
              .unwrap_or(floating_defaults.shown_on_top),
            transparency: floating_defaults.transparency.clone(),
          });

          let window = update_window_state(
            window.clone(),
            window.toggled_state(target_state, config),
            state,
            config,
          )?;

          if !window.has_custom_floating_placement() && centered {
            set_window_position(
              window,
              &WindowPositionTarget::Centered,
              state,
            )?;
          }

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::ToggleFullscreen {
        maximized,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let fullscreen_defaults =
            &config.value.window_behavior.state_defaults.fullscreen;

          let target_state =
            WindowState::Fullscreen(FullscreenStateConfig {
              maximized: maximized
                .unwrap_or(fullscreen_defaults.maximized),
              shown_on_top: shown_on_top
                .unwrap_or(fullscreen_defaults.shown_on_top),
              transparency: fullscreen_defaults.transparency.clone(),
            });

          update_window_state(
            window.clone(),
            window.toggled_state(target_state, config),
            state,
            config,
          )?;

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::ToggleMinimized => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window.clone(),
              window.toggled_state(WindowState::Minimized, config),
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleTiling => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window.clone(),
              window.toggled_state(WindowState::Tiling, config),
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      // LINT: `opacity` is only used on Windows.
      #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
      InvokeCommand::ToggleTransparencyPin { opacity } => {
        match subject_container.as_window_container() {
          #[cfg(target_os = "windows")]
          Ok(window) => {
            if window.transparency_pin().is_some() {
              // Unpin. Queue an effects update so the window reverts to
              // the transparency from the user's `window_effects` config.
              window.set_transparency_pin(None);
              state.pending_sync.queue_all_effects_update();
            } else {
              // Pin at the given opacity (fully opaque if omitted). The
              // pin overrides transparency effects until unpinned.
              let opacity = opacity
                .clone()
                .unwrap_or_else(|| OpacityValue::from_alpha(u8::MAX));

              _ = window.native().set_transparency(&opacity);
              window.set_transparency_pin(Some(opacity));
            }

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleTilingDirection => {
        toggle_tiling_direction(subject_container, state, config)
      }
      InvokeCommand::SetTilingDirection { tiling_direction } => {
        set_tiling_direction(
          subject_container,
          state,
          config,
          tiling_direction,
        )
      }
      InvokeCommand::WmCycleFocus {
        omit_floating,
        omit_fullscreen,
        omit_minimized,
        omit_tiling,
      } => cycle_focus(
        *omit_floating,
        *omit_fullscreen,
        *omit_minimized,
        *omit_tiling,
        state,
        config,
      ),
      InvokeCommand::WmDisableBindingMode { name } => {
        disable_binding_mode(name, state);
        Ok(())
      }
      InvokeCommand::WmEnableBindingMode { name } => {
        enable_binding_mode(name, state, config)
      }
      InvokeCommand::WmExit => state.emit_exit(),
      InvokeCommand::WmRedraw => {
        state
          .pending_sync
          .queue_container_to_redraw(state.root_container.clone());

        Ok(())
      }
      InvokeCommand::WmReloadConfig => reload_config(state, config),
      InvokeCommand::WmTogglePause => {
        toggle_pause(state);
        Ok(())
      }
    }
  }

  /// Runs cleanup tasks when the WM is exiting.
  pub(crate) fn cleanup(
    &mut self,
    config: &mut UserConfig,
    ipc_server: &mut IpcServer,
  ) {
    self.state.emit_event(WmEvent::ApplicationExiting);

    // Ensure that the WM is unpaused, otherwise, shutdown commands won't
    // get executed.
    self.state.is_paused = false;

    // Run user's shutdown commands.
    if let Err(err) = self.process_commands(
      &config.value.general.shutdown_commands.clone(),
      None,
      config,
    ) {
      tracing::warn!("Failed to run shutdown commands: {:?}", err);
    }

    // Emit remaining WM events before exiting.
    while let Ok(wm_event) = self.event_rx.try_recv() {
      tracing::info!(
        "Emitting WM event before shutting down: {:?}",
        wm_event
      );

      if let Err(err) = ipc_server.process_event(wm_event) {
        tracing::warn!("{:?}", err);
      }
    }
  }
}

fn auto_tiling_window_id(event: &WmEvent) -> Option<Uuid> {
  let container = match event {
    WmEvent::FocusChanged { focused_container }
    | WmEvent::FocusedContainerMoved { focused_container } => {
      focused_container
    }
    _ => return None,
  };

  match container {
    ContainerDto::Window(window) => Some(window.id),
    _ => None,
  }
}

fn auto_tiling_direction(
  width: i32,
  height: i32,
) -> Option<TilingDirection> {
  match width.cmp(&height) {
    std::cmp::Ordering::Less => Some(TilingDirection::Vertical),
    std::cmp::Ordering::Greater => Some(TilingDirection::Horizontal),
    std::cmp::Ordering::Equal => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use wm_common::{DisplayState, WindowDto};
  use wm_platform::{Rect, RectDelta};

  #[test]
  fn extracts_window_id_from_supported_events() {
    let window_id = Uuid::new_v4();
    let event = WmEvent::FocusChanged {
      focused_container: ContainerDto::Window(WindowDto {
        id: window_id,
        parent_id: None,
        has_focus: true,
        tiling_size: Some(1.0),
        width: 800,
        height: 600,
        x: 0,
        y: 0,
        state: WindowState::Tiling,
        prev_state: None,
        display_state: DisplayState::Shown,
        border_delta: RectDelta::zero(),
        floating_placement: Rect::from_xy(0, 0, 800, 600),
        handle: 0,
        title: String::new(),
        #[cfg(target_os = "windows")]
        class_name: String::new(),
        process_name: String::new(),
        active_drag: None,
        transparency_pin: None,
      }),
    };

    assert_eq!(auto_tiling_window_id(&event), Some(window_id));
  }

  #[test]
  fn chooses_vertical_for_taller_windows() {
    assert_eq!(
      auto_tiling_direction(600, 900),
      Some(TilingDirection::Vertical)
    );
  }

  #[test]
  fn chooses_horizontal_for_wider_windows() {
    assert_eq!(
      auto_tiling_direction(1200, 700),
      Some(TilingDirection::Horizontal)
    );
  }

  #[test]
  fn skips_square_windows() {
    assert_eq!(auto_tiling_direction(800, 800), None);
  }
}
