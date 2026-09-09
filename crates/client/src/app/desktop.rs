//! One native application owns the manager and every session window on macOS.
//! Network setup stays on workers; all window creation and input stay on the
//! main thread. This host is portable so its integration can be tested on CI.

use super::*;
use crate::{launcher::Launcher, profiles::Profile};
use eframe::egui;
use std::path::PathBuf;
use winit::keyboard::{KeyCode, PhysicalKey};

type Connected = (Client, AppOptions, Session);

pub fn run(path: Option<PathBuf>, initial: Option<Connected>) -> Result<Option<String>> {
    let event_loop = EventLoop::<Wake>::with_user_event().build()?;
    #[cfg(unix)]
    let broker = path
        .as_ref()
        .map(|_| crate::askpass::broker::Broker::new())
        .transpose()?;
    let mut desktop = Desktop {
        #[cfg(unix)]
        broker,
        launcher: path.map(|path| {
            let mut launcher = Launcher::new(path);
            launcher.use_shared_windows();
            launcher
        }),
        manager: None,
        viewers: Vec::new(),
        pending: Vec::new(),
        initial,
        proxy: event_loop.create_proxy(),
        modifiers: ModifiersState::empty(),
        swallowed: Vec::new(),
        last_reason: None,
    };
    event_loop.run_app(&mut desktop)?;
    Ok(desktop.last_reason)
}

struct Pending {
    name: String,
    result: crossbeam_channel::Receiver<Result<Connected>>,
}

struct Desktop {
    #[cfg(unix)]
    broker: Option<crate::askpass::broker::Broker>,
    launcher: Option<Launcher>,
    manager: Option<ManagerWindow>,
    viewers: Vec<App>,
    pending: Vec<Pending>,
    initial: Option<Connected>,
    proxy: EventLoopProxy<Wake>,
    modifiers: ModifiersState,
    swallowed: Vec<PhysicalKey>,
    last_reason: Option<String>,
}

impl Desktop {
    fn add_viewer(&mut self, event_loop: &ActiveEventLoop, connected: Connected) -> Result<()> {
        let (client, opts, session) = connected;
        *session.waker.lock().unwrap() = Some(self.proxy.clone());
        let mut app = App::new(client, opts, session);
        app.shared_window = true;
        app.init_window(event_loop)?;
        self.viewers.push(app);
        Ok(())
    }

    fn report(&mut self, message: String) {
        log::error!("{message}");
        if let Some(launcher) = &mut self.launcher {
            launcher.connection_failed(message);
        }
        if let Some(manager) = &self.manager {
            manager.window.set_visible(true);
            manager.window.request_redraw();
        }
    }

    fn start(&mut self, profile: Profile) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let proxy = self.proxy.clone();
        #[allow(unused_mut)]
        let mut ssh_env = crate::askpass::ssh_env();
        #[cfg(unix)]
        if let Some(broker) = &self.broker {
            ssh_env.push((
                crate::askpass::broker::SOCKET_ENV.into(),
                broker.path.to_string_lossy().into_owned(),
            ));
        }
        self.pending.push(Pending {
            name: profile.name.clone(),
            result: rx,
        });
        std::thread::spawn(move || {
            let result = connect_profile(&profile, &proxy, ssh_env);
            let _ = tx.send(result);
            let _ = proxy.send_event(Wake);
        });
    }

    fn poll(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(unix)]
        if self.broker.as_mut().is_some_and(|b| b.poll()) {
            if let Some(manager) = &self.manager {
                manager.window.set_visible(true);
                manager.window.focus_window();
                manager.window.request_redraw();
            }
        }
        let mut waiting = Vec::new();
        for pending in std::mem::take(&mut self.pending) {
            match pending.result.try_recv() {
                Ok(Ok(connected)) => {
                    if let Err(e) = self.add_viewer(event_loop, connected) {
                        self.report(format!("Could not open {}: {e:#}", pending.name));
                    }
                }
                Ok(Err(e)) => self.report(format!("Could not connect to {}: {e:#}", pending.name)),
                Err(crossbeam_channel::TryRecvError::Empty) => waiting.push(pending),
                Err(_) => self.report(format!(
                    "Connection to {} stopped unexpectedly",
                    pending.name
                )),
            }
        }
        self.pending = waiting;
        for viewer in &mut self.viewers {
            viewer.user_event(event_loop, Wake);
            if viewer.exit_reason.is_none() {
                viewer.housekeeping();
                if viewer.dirty.is_some() || viewer.full_redraw {
                    viewer.request_redraw();
                }
            }
        }
        let mut live = Vec::new();
        for mut viewer in std::mem::take(&mut self.viewers) {
            if viewer.exit_reason.is_some() {
                viewer.exiting(event_loop);
                self.last_reason = viewer.exit_reason.take();
            } else {
                live.push(viewer);
            }
        }
        self.viewers = live;
        if let Some(launcher) = &mut self.launcher {
            let profiles = launcher.take_connections(self.viewers.len(), self.pending.len());
            for profile in profiles {
                self.start(profile);
            }
        }
        if self.viewers.is_empty() && self.pending.is_empty() {
            if let Some(manager) = &self.manager {
                if manager.window.is_visible() == Some(false) {
                    manager.window.set_visible(true);
                }
            } else {
                event_loop.exit();
            }
        }
    }
}

fn connect_profile(
    profile: &Profile,
    proxy: &EventLoopProxy<Wake>,
    ssh_env: Vec<(String, String)>,
) -> Result<Connected> {
    use crate::tunnel::{Endpoint, RemoteTarget, TunnelConfig};
    let mut endpoint = Endpoint::ssh(
        TunnelConfig {
            destination: profile.destination(),
            ssh_port: profile.ssh_port,
            identity: profile.identity.clone(),
            options: profile.ssh_options.clone(),
            remote: RemoteTarget::Port(profile.remote_port.unwrap_or(lynxrdp_proto::DEFAULT_PORT)),
            env: ssh_env,
            ..Default::default()
        },
        Duration::from_secs(120),
    );
    let connect = ConnectOptions {
        size: profile.size,
        ..Default::default()
    };
    let (waker, slot) = make_waker();
    *slot.lock().unwrap() = Some(proxy.clone());
    let client = Client::from_stream(endpoint.connect()?.into_tcp(), &connect, Some(waker))?;
    Ok((
        client,
        AppOptions {
            title: profile.name.clone(),
            fullscreen: profile.fullscreen,
            dynamic_resize: profile.dynamic_resize,
            clipboard: profile.clipboard,
            scale: profile.scale,
        },
        Session {
            endpoint: Some(endpoint),
            connect,
            waker: slot,
        },
    ))
}

impl ApplicationHandler<Wake> for Desktop {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.manager.is_none() {
            if let Some(launcher) = &self.launcher {
                match ManagerWindow::new(event_loop, launcher, self.proxy.clone()) {
                    Ok(manager) => self.manager = Some(manager),
                    Err(e) => {
                        self.report(format!("Could not open connection manager: {e:#}"));
                        event_loop.exit();
                    }
                }
            }
        }
        if let Some(initial) = self.initial.take() {
            if let Err(e) = self.add_viewer(event_loop, initial) {
                self.report(format!("Could not open session: {e:#}"));
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _: Wake) {
        self.poll(event_loop);
        if let Some(manager) = &self.manager {
            if manager.drawable() && manager.ctx.has_requested_repaint() {
                manager.window.request_redraw();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let WindowEvent::ModifiersChanged(m) = &event {
            self.modifiers = m.state();
        }
        if let WindowEvent::KeyboardInput { event: key, .. } = &event {
            if key.state == ElementState::Released && self.swallowed.contains(&key.physical_key) {
                self.swallowed.retain(|k| *k != key.physical_key);
                return;
            }
            if self.modifiers.super_key() && key.state == ElementState::Pressed {
                if key.physical_key == PhysicalKey::Code(KeyCode::KeyQ) {
                    event_loop.exit();
                    return;
                }
                if key.physical_key == PhysicalKey::Code(KeyCode::KeyW) {
                    if !self.swallowed.contains(&key.physical_key) {
                        self.swallowed.push(key.physical_key);
                    }
                    self.window_event(event_loop, id, WindowEvent::CloseRequested);
                    return;
                }
            }
        }
        if self.manager.as_ref().is_some_and(|m| m.window.id() == id) {
            let manager = self.manager.as_mut().unwrap();
            match event {
                WindowEvent::CloseRequested => {
                    #[cfg(unix)]
                    if self.broker.as_mut().is_some_and(|broker| broker.cancel()) {
                        manager.window.request_redraw();
                        return;
                    }
                    if self.viewers.is_empty() && self.pending.is_empty() {
                        event_loop.exit();
                    } else {
                        manager.window.set_visible(false);
                    }
                }
                WindowEvent::RedrawRequested => {
                    if let Some(launcher) = &mut self.launcher {
                        match manager.draw(
                            launcher,
                            #[cfg(unix)]
                            self.broker.as_mut(),
                        ) {
                            Ok(true) => event_loop.exit(),
                            Ok(false) => {}
                            Err(e) => self.report(format!("Drawing connection manager: {e:#}")),
                        }
                    }
                }
                _ => {
                    if manager
                        .state
                        .on_window_event(&manager.window, &event)
                        .repaint
                    {
                        manager.window.request_redraw();
                    }
                }
            }
        } else if let Some(viewer) = self
            .viewers
            .iter_mut()
            .find(|v| v.gfx.as_ref().is_some_and(|g| g.window.id() == id))
        {
            viewer.window_event(event_loop, id, event);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.poll(event_loop);
        let now = Instant::now();
        let mut wake = now + Duration::from_millis(250);
        for viewer in &self.viewers {
            wake = wake.min(now + viewer.next_wake());
        }
        if let Some(manager) = self.manager.as_ref().filter(|m| m.drawable()) {
            if now >= manager.repaint_at {
                manager.window.request_redraw();
            } else {
                wake = wake.min(manager.repaint_at);
            }
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(wake));
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(unix)]
        self.broker.take();
        for pending in self.pending.drain(..) {
            let _ = pending.result.recv_timeout(Duration::from_millis(250));
        }
        for viewer in &mut self.viewers {
            viewer.exiting(event_loop);
        }
    }
}

struct ManagerWindow {
    window: Arc<Window>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: crate::gui_paint::Renderer,
    info: egui::ViewportInfo,
    repaint_at: Instant,
}

impl ManagerWindow {
    fn drawable(&self) -> bool {
        self.window.is_visible() != Some(false) && self.window.is_minimized() != Some(true)
    }

    fn new(
        event_loop: &ActiveEventLoop,
        launcher: &Launcher,
        proxy: EventLoopProxy<Wake>,
    ) -> Result<Self> {
        // macOS gets the Dock icon from the application bundle, exactly as
        // session viewers do. A window icon must not replace NSApplication's icon.
        let window = Arc::new(
            event_loop.create_window(
                Window::default_attributes()
                    .with_title("LynxRDP")
                    .with_inner_size(winit::dpi::LogicalSize::new(880.0, 560.0))
                    .with_min_inner_size(winit::dpi::LogicalSize::new(640.0, 420.0)),
            )?,
        );
        let context =
            softbuffer::Context::new(window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let surface = softbuffer::Surface::new(&context, window.clone())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let ctx = egui::Context::default();
        launcher.install(&ctx);
        ctx.set_request_repaint_callback(move |info| {
            if info.delay.is_zero() {
                let _ = proxy.send_event(Wake);
            }
        });
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        Ok(Self {
            window,
            surface,
            ctx,
            state,
            renderer: Default::default(),
            info: Default::default(),
            repaint_at: Instant::now(),
        })
    }

    fn draw(
        &mut self,
        launcher: &mut Launcher,
        #[cfg(unix)] mut broker: Option<&mut crate::askpass::broker::Broker>,
    ) -> Result<bool> {
        if !self.drawable() {
            return Ok(false);
        }
        let size = self.window.inner_size();
        let (Some(w), Some(h)) = (
            std::num::NonZeroU32::new(size.width),
            std::num::NonZeroU32::new(size.height),
        ) else {
            return Ok(false);
        };
        egui_winit::update_viewport_info(&mut self.info, &self.ctx, &self.window, false);
        let mut input = self.state.take_egui_input(&self.window);
        input
            .viewports
            .insert(egui::ViewportId::ROOT, self.info.clone());
        let mut output = self.ctx.run(input, |ctx| {
            launcher.draw(ctx);
            #[cfg(unix)]
            if let Some(broker) = broker.as_deref_mut() {
                broker.show(ctx);
            }
        });
        self.state
            .handle_platform_output(&self.window, std::mem::take(&mut output.platform_output));
        let mut close = false;
        if let Some(viewport) = output.viewport_output.get_mut(&egui::ViewportId::ROOT) {
            close = viewport
                .commands
                .iter()
                .any(|c| matches!(c, egui::ViewportCommand::Close));
            self.repaint_at =
                Instant::now() + viewport.repaint_delay.min(Duration::from_millis(500));
            egui_winit::process_viewport_commands(
                &self.ctx,
                &mut self.info,
                std::mem::take(&mut viewport.commands),
                &self.window,
                &mut Default::default(),
            );
        }
        self.surface
            .resize(w, h)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut buffer = self
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        buffer.fill(0);
        self.renderer
            .paint(&self.ctx, output, &mut buffer, size.width, size.height);
        buffer.present().map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(close)
    }
}

// The one test left here drives a real X11 display, so the whole module is
// gated the same way the test is rather than leaving an import with nothing to
// bring in on the platform this host actually ships on.
#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an isolated X11 display (run under Xvfb)"]
    fn shared_host_closes_one_viewer_without_exiting_the_application() {
        use winit::platform::x11::EventLoopBuilderExtX11;
        let event_loop = EventLoop::<Wake>::with_user_event()
            .with_x11()
            .with_any_thread(true)
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut launcher = Launcher::new(dir.path().join("connections.toml"));
        launcher.use_shared_windows();
        let desktop = Desktop {
            broker: None,
            launcher: Some(launcher),
            manager: None,
            viewers: Vec::new(),
            pending: Vec::new(),
            initial: None,
            proxy: event_loop.create_proxy(),
            modifiers: ModifiersState::empty(),
            swallowed: Vec::new(),
            last_reason: None,
        };
        struct Check(Desktop);
        impl ApplicationHandler<Wake> for Check {
            fn resumed(&mut self, event_loop: &ActiveEventLoop) {
                self.0.resumed(event_loop);
                for _ in 0..2 {
                    let (addr, _) = super::super::tests::fake_session(1);
                    let mut app = super::super::tests::test_app(addr, None);
                    app.shared_window = true;
                    app.init_window(event_loop).unwrap();
                    self.0.viewers.push(app);
                }
                assert!(self.0.manager.is_some());
                assert_eq!(self.0.viewers.len(), 2);
                let first = self.0.viewers[0].gfx.as_ref().unwrap().window.id();
                let second = self.0.viewers[1].gfx.as_ref().unwrap().window.id();
                assert_ne!(first, second);
                self.0
                    .window_event(event_loop, first, WindowEvent::CloseRequested);
                self.0.poll(event_loop);
                assert!(!event_loop.exiting());
                assert_eq!(self.0.viewers.len(), 1);
                assert_eq!(self.0.viewers[0].gfx.as_ref().unwrap().window.id(), second);
                self.0.viewers[0].on_link_lost("The desktop session has ended.".into());
                self.0.poll(event_loop);
                assert!(self.0.viewers.is_empty());
                assert!(self.0.manager.is_some());
                assert!(!event_loop.exiting());
                self.0
                    .manager
                    .as_mut()
                    .unwrap()
                    .draw(self.0.launcher.as_mut().unwrap(), None)
                    .unwrap();
                event_loop.exit();
            }
            fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
        }
        event_loop.run_app(&mut Check(desktop)).unwrap();
    }
}
