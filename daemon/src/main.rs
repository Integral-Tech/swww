//! All expects in this program must be carefully chosen on purpose. The idea is that if any of
//! them fail there is no point in continuing. All of the initialization code, for example, is full
//! of `expects`, **on purpose**, because we **want** to unwind and exit when they happen

mod animations;
mod cli;
mod wallpaper;
mod wayland;
use log::{debug, error, info, warn, LevelFilter};
use rustix::{
    event::{Nsecs, Secs},
    fd::OwnedFd,
    fs::Timespec,
};

use wallpaper::Wallpaper;

use waybackend::{objman, types::ObjectId, wire, Global};
use wayland::zwlr_layer_shell_v1::Layer;

use std::{
    cell::RefCell,
    fs,
    io::{IsTerminal, Write},
    num::NonZeroI32,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use animations::{ImageAnimator, TransitionAnimator};
use common::ipc::{
    Answer, BgInfo, ImageReq, IpcSocket, PixelFormat, RequestRecv, RequestSend, Scale, Server,
};
use common::mmap::MmappedStr;

// We need this because this might be set by signals, so we can't keep it in the daemon
static EXIT: AtomicBool = AtomicBool::new(false);

fn exit_daemon() {
    EXIT.store(true, Ordering::Relaxed);
}

fn should_daemon_exit() -> bool {
    EXIT.load(Ordering::Relaxed)
}

extern "C" fn signal_handler(_s: libc::c_int) {
    exit_daemon();
}

struct Daemon {
    backend: waybackend::Waybackend,
    objman: objman::ObjectManager<WaylandObject>,
    registry: ObjectId,
    compositor: ObjectId,
    shm: ObjectId,
    viewporter: ObjectId,
    layer_shell: ObjectId,
    layer: Layer,
    pixel_format: PixelFormat,
    wallpapers: Vec<Rc<RefCell<Wallpaper>>>,
    transition_animators: Vec<TransitionAnimator>,
    image_animators: Vec<ImageAnimator>,
    namespace: String,
    use_cache: bool,
    fractional_scale_manager: Option<ObjectId>,

    /// We use PollTime as a way of making sure we draw at the right time.
    /// when we call `Daemon::draw` before the frame callback returned, we need to *not* draw and
    /// instead wait for the next callback, which we do with a short poll time.
    poll_time: Option<Timespec>,

    forced_shm_format: bool,

    // This are the globals we have received before we figured out which shm format to use
    output_globals: Option<Vec<Global>>,
    // This callback is from the sync request we make when calling `Daemon::new`
    callback: Option<ObjectId>,
}

impl Daemon {
    fn new(
        mut backend: waybackend::Waybackend,
        mut objman: objman::ObjectManager<WaylandObject>,
        args: cli::Cli,
        output_globals: Vec<Global>,
    ) -> Self {
        let registry = objman.get_first(WaylandObject::Registry).unwrap();
        let compositor = objman.get_first(WaylandObject::Compositor).unwrap();
        let shm = objman.get_first(WaylandObject::Shm).unwrap();
        let layer_shell = objman.get_first(WaylandObject::LayerShell).unwrap();
        let viewporter = objman.get_first(WaylandObject::Viewporter).unwrap();
        let fractional_scale_manager = objman.get_first(WaylandObject::FractionalScaler);

        let callback = objman.create(WaylandObject::Callback);
        wayland::wl_display::req::sync(&mut backend, waybackend::WL_DISPLAY, callback).unwrap();

        Self {
            backend,
            objman,
            registry,
            compositor,
            shm,
            viewporter,
            layer_shell,
            layer: args.layer,
            pixel_format: args.format.unwrap_or(PixelFormat::Xrgb),
            wallpapers: Vec::new(),
            transition_animators: Vec::new(),
            image_animators: Vec::new(),
            namespace: args.namespace,
            use_cache: !args.no_cache,
            fractional_scale_manager,
            poll_time: None,
            forced_shm_format: args.format.is_some(),
            output_globals: Some(output_globals),
            callback: Some(callback),
        }
    }

    /// always sets the poll time to the smalest value
    fn set_poll_time(&mut self, new_time: Timespec) {
        match self.poll_time {
            None => self.poll_time = Some(new_time),
            Some(t1) => {
                if new_time < t1 {
                    self.poll_time = Some(new_time)
                }
            }
        }
    }

    fn new_output(&mut self, output_name: u32) {
        let wallpaper = Rc::new(RefCell::new(Wallpaper::new(self, self.layer, output_name)));
        self.wallpapers.push(wallpaper);
    }

    fn recv_socket_msg(&mut self, stream: IpcSocket<Server>) {
        let bytes = match stream.recv() {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("FATAL: cannot read socket: {e}. Exiting...");
                exit_daemon();
                return;
            }
        };
        let request = RequestRecv::receive(bytes);
        let answer = match request {
            RequestRecv::Clear(clear) => {
                let wallpapers = self.find_wallpapers_by_names(&clear.outputs);
                self.stop_animations(&wallpapers);
                for wallpaper in &wallpapers {
                    let mut wallpaper = wallpaper.borrow_mut();
                    wallpaper.set_img_info(common::ipc::BgImg::Color(clear.color));
                    wallpaper.clear(
                        &mut self.backend,
                        &mut self.objman,
                        self.pixel_format,
                        clear.color,
                    );
                }
                crate::wallpaper::attach_buffers_and_damage_surfaces(
                    &mut self.backend,
                    &mut self.objman,
                    &wallpapers,
                );
                crate::wallpaper::commit_wallpapers(&mut self.backend, &wallpapers);
                Answer::Ok
            }
            RequestRecv::Ping => Answer::Ping(self.wallpapers.iter().all(|w| {
                w.borrow()
                    .configured
                    .load(std::sync::atomic::Ordering::Acquire)
            })),
            RequestRecv::Kill => {
                exit_daemon();
                Answer::Ok
            }
            RequestRecv::Query => Answer::Info(self.wallpapers_info()),
            RequestRecv::Img(ImageReq {
                transition,
                mut imgs,
                mut outputs,
                mut animations,
            }) => {
                while !imgs.is_empty() && !outputs.is_empty() {
                    let names = outputs.pop().unwrap();
                    let img = imgs.pop().unwrap();
                    let animation = if let Some(ref mut animations) = animations {
                        animations.pop()
                    } else {
                        None
                    };
                    let wallpapers = self.find_wallpapers_by_names(&names);
                    self.stop_animations(&wallpapers);
                    if let Some(mut transition) = TransitionAnimator::new(
                        wallpapers,
                        &transition,
                        self.pixel_format,
                        img,
                        animation,
                    ) {
                        transition.frame(&mut self.backend, &mut self.objman, self.pixel_format);
                        self.transition_animators.push(transition);
                    }
                }
                self.set_poll_time(Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                });
                Answer::Ok
            }
        };
        if let Err(e) = answer.send(&stream) {
            error!("error sending answer to client: {e}");
        }
    }

    fn wallpapers_info(&self) -> Box<[BgInfo]> {
        self.wallpapers
            .iter()
            .map(|wallpaper| wallpaper.borrow().get_bg_info(self.pixel_format))
            .collect()
    }

    fn find_wallpapers_by_names(&self, names: &[MmappedStr]) -> Vec<Rc<RefCell<Wallpaper>>> {
        self.wallpapers
            .iter()
            .filter_map(|wallpaper| {
                if names.is_empty() || names.iter().any(|n| wallpaper.borrow().has_name(n.str())) {
                    return Some(Rc::clone(wallpaper));
                }
                None
            })
            .collect()
    }

    fn draw(&mut self) {
        self.poll_time = None;

        let mut i = 0;
        while i < self.transition_animators.len() {
            let animator = &mut self.transition_animators[i];
            if animator
                .wallpapers
                .iter()
                .all(|w| w.borrow().is_draw_ready())
            {
                let time = animator.time_to_draw();
                if time > Duration::from_micros(1000) {
                    self.set_poll_time(Timespec {
                        tv_sec: time.as_secs() as Secs,
                        tv_nsec: time.subsec_nanos().saturating_sub(500_000) as Nsecs,
                    });
                    i += 1;
                    continue;
                }

                if !time.is_zero() {
                    spin_sleep(time);
                }

                wallpaper::attach_buffers_and_damage_surfaces(
                    &mut self.backend,
                    &mut self.objman,
                    &animator.wallpapers,
                );

                wallpaper::commit_wallpapers(&mut self.backend, &animator.wallpapers);
                animator.updt_time();
                if animator.frame(&mut self.backend, &mut self.objman, self.pixel_format) {
                    let animator = self.transition_animators.swap_remove(i);
                    if let Some(anim) = animator.into_image_animator() {
                        self.image_animators.push(anim);
                    }
                    continue;
                }
            }
            let time = animator.time_to_draw();
            self.set_poll_time(Timespec {
                tv_sec: time.as_secs() as Secs,
                tv_nsec: time.subsec_nanos().saturating_sub(500_000) as Nsecs,
            });
            i += 1;
        }

        self.image_animators.retain(|a| !a.wallpapers.is_empty());
        let mut i = 0;
        while i < self.image_animators.len() {
            let animator = &mut self.image_animators[i];
            if animator
                .wallpapers
                .iter()
                .all(|w| w.borrow().is_draw_ready())
            {
                let time = animator.time_to_draw();
                if time > Duration::from_micros(1000) {
                    self.set_poll_time(Timespec {
                        tv_sec: time.as_secs() as Secs,
                        tv_nsec: time.subsec_nanos().saturating_sub(500_000) as Nsecs,
                    });
                    i += 1;
                    continue;
                }

                if !time.is_zero() {
                    spin_sleep(time);
                }

                wallpaper::attach_buffers_and_damage_surfaces(
                    &mut self.backend,
                    &mut self.objman,
                    &animator.wallpapers,
                );
                wallpaper::commit_wallpapers(&mut self.backend, &animator.wallpapers);
                animator.updt_time();
                animator.frame(&mut self.backend, &mut self.objman, self.pixel_format);
            }
            let time = animator.time_to_draw();
            self.set_poll_time(Timespec {
                tv_sec: time.as_secs() as Secs,
                tv_nsec: time.subsec_nanos().saturating_sub(500_000) as Nsecs,
            });
            i += 1;
        }
    }

    fn stop_animations(&mut self, wallpapers: &[Rc<RefCell<Wallpaper>>]) {
        for transition in self.transition_animators.iter_mut() {
            transition
                .wallpapers
                .retain(|w1| !wallpapers.iter().any(|w2| w1.borrow().eq(&w2.borrow())));
        }

        for animator in self.image_animators.iter_mut() {
            animator
                .wallpapers
                .retain(|w1| !wallpapers.iter().any(|w2| w1.borrow().eq(&w2.borrow())));
        }

        self.transition_animators
            .retain(|t| !t.wallpapers.is_empty());

        self.image_animators.retain(|a| !a.wallpapers.is_empty());
    }
}

impl wayland::wl_display::EvHandler for Daemon {
    fn delete_id(&mut self, _: ObjectId, id: u32) {
        if let Ok(id) = ObjectId::try_new(id) {
            self.objman.remove(id);
        }
    }

    fn error(&mut self, _: ObjectId, object_id: ObjectId, code: u32, message: &str) {
        error!("WAYLAND PROTOCOL ERROR: object: {object_id}, code: {code}, message: {message}");
        exit_daemon();
    }
}

impl wayland::wl_registry::EvHandler for Daemon {
    fn global(&mut self, _: ObjectId, name: u32, interface: &str, version: u32) {
        if interface == "wl_output" {
            if version < 4 {
                error!("your compositor must support at least version 4 of wl_output");
            } else {
                self.new_output(name);
            }
        }
    }

    fn global_remove(&mut self, _: ObjectId, name: u32) {
        if let Some(i) = self
            .wallpapers
            .iter()
            .position(|w| w.borrow().has_output_name(name))
        {
            let w = self.wallpapers.remove(i);
            w.borrow_mut().destroy(&mut self.backend);
            self.stop_animations(&[w]);
        }
    }
}

impl wayland::wl_shm::EvHandler for Daemon {
    fn format(&mut self, _: ObjectId, format: wayland::wl_shm::Format) {
        use wayland::wl_shm::Format;
        match format {
            Format::xrgb8888 => debug!("available shm format: Xrbg"),
            Format::xbgr8888 => {
                debug!("available shm format: Xbgr");
                if !self.forced_shm_format && self.pixel_format == PixelFormat::Xrgb {
                    self.pixel_format = PixelFormat::Xbgr;
                }
            }
            Format::rgb888 => {
                debug!("available shm format: Rbg");
                if !self.forced_shm_format && self.pixel_format != PixelFormat::Bgr {
                    self.pixel_format = PixelFormat::Rgb
                }
            }
            Format::bgr888 => {
                debug!("available shm format: Bgr");
                if !self.forced_shm_format {
                    self.pixel_format = PixelFormat::Bgr
                }
            }
            _ => (),
        }
    }
}

impl wayland::wl_output::EvHandler for Daemon {
    fn geometry(
        &mut self,
        sender_id: ObjectId,
        _x: i32,
        _y: i32,
        _physical_width: i32,
        _physical_height: i32,
        _subpixel: wayland::wl_output::Subpixel,
        _make: &str,
        _model: &str,
        transform: wayland::wl_output::Transform,
    ) {
        for wallpaper in self.wallpapers.iter() {
            let mut wallpaper = wallpaper.borrow_mut();
            if wallpaper.has_output(sender_id) {
                if transform == wayland::wl_output::Transform::flipped_270 {
                    error!("received invalid transform value from compositor: {transform:?}")
                } else {
                    wallpaper.set_transform(transform);
                }
                break;
            }
        }
    }

    fn mode(
        &mut self,
        sender_id: ObjectId,
        flags: wayland::wl_output::Mode,
        width: i32,
        height: i32,
        _refresh: i32,
    ) {
        // the protocol states we should not rely on non-current modes
        if flags.contains(wayland::wl_output::Mode::CURRENT) {
            for wallpaper in self.wallpapers.iter() {
                let mut wallpaper = wallpaper.borrow_mut();
                if wallpaper.has_output(sender_id) {
                    wallpaper.set_dimensions(width, height);
                    break;
                }
            }
        }
    }

    fn done(&mut self, sender_id: ObjectId) {
        for wallpaper in self.wallpapers.iter() {
            if wallpaper.borrow().has_output(sender_id) {
                if wallpaper.borrow_mut().commit_surface_changes(
                    &mut self.backend,
                    &mut self.objman,
                    &self.namespace,
                    self.use_cache,
                ) {
                    self.stop_animations(&[wallpaper.clone()]);
                }
                break;
            }
        }
    }

    fn scale(&mut self, sender_id: ObjectId, factor: i32) {
        for wallpaper in self.wallpapers.iter() {
            let mut wallpaper = wallpaper.borrow_mut();
            if wallpaper.has_output(sender_id) {
                match NonZeroI32::new(factor) {
                    Some(factor) => wallpaper.set_scale(Scale::Output(factor)),
                    None => error!("received scale factor of 0 from compositor"),
                }
                break;
            }
        }
    }

    fn name(&mut self, sender_id: ObjectId, name: &str) {
        for wallpaper in self.wallpapers.iter() {
            let mut wallpaper = wallpaper.borrow_mut();
            if wallpaper.has_output(sender_id) {
                wallpaper.set_name(name.to_string());
                break;
            }
        }
    }

    fn description(&mut self, sender_id: ObjectId, description: &str) {
        for wallpaper in self.wallpapers.iter() {
            let mut wallpaper = wallpaper.borrow_mut();
            if wallpaper.has_output(sender_id) {
                wallpaper.set_desc(description.to_string());
                break;
            }
        }
    }
}

impl wayland::wl_surface::EvHandler for Daemon {
    fn enter(&mut self, _sender_id: ObjectId, output: ObjectId) {
        debug!("Output {}: Surface Enter", output.get());
    }

    fn leave(&mut self, _sender_id: ObjectId, output: ObjectId) {
        debug!("Output {}: Surface Leave", output.get());
    }

    fn preferred_buffer_scale(&mut self, sender_id: ObjectId, factor: i32) {
        for wallpaper in self.wallpapers.iter() {
            let mut wallpaper = wallpaper.borrow_mut();
            if wallpaper.has_surface(sender_id) {
                match NonZeroI32::new(factor) {
                    Some(factor) => wallpaper.set_scale(Scale::Preferred(factor)),
                    None => error!("received scale factor of 0 from compositor"),
                }
                break;
            }
        }
    }

    fn preferred_buffer_transform(
        &mut self,
        _sender_id: ObjectId,
        _transform: wayland::wl_output::Transform,
    ) {
        //Ignore these for now
    }
}

impl wayland::wl_region::EvHandler for Daemon {}

impl wayland::wl_buffer::EvHandler for Daemon {
    fn release(&mut self, sender_id: ObjectId) {
        for wallpaper in self.wallpapers.iter() {
            let strong_count = Rc::strong_count(wallpaper);
            if wallpaper.borrow_mut().try_set_buffer_release_flag(
                &mut self.backend,
                sender_id,
                strong_count,
            ) {
                return;
            }
        }
        error!("We failed to find wayland buffer with id: {sender_id}. This should be impossible.");
    }
}

impl wayland::wl_callback::EvHandler for Daemon {
    fn done(&mut self, sender_id: ObjectId, _callback_data: u32) {
        if self.callback.is_some_and(|obj| obj == sender_id) {
            info!("selected pixel format: {:?}", self.pixel_format);

            let output_globals = self.output_globals.take();
            for output in output_globals.unwrap() {
                self.new_output(output.name());
            }
            self.callback = None;

            return;
        }

        for wallpaper in self.wallpapers.iter() {
            if wallpaper.borrow().has_callback(sender_id) {
                wallpaper.borrow_mut().frame_callback_completed();
                break;
            }
        }
    }
}

impl wayland::wl_compositor::EvHandler for Daemon {}
impl wayland::wl_shm_pool::EvHandler for Daemon {}

impl wayland::zwlr_layer_shell_v1::EvHandler for Daemon {}
impl wayland::zwlr_layer_surface_v1::EvHandler for Daemon {
    fn configure(&mut self, sender_id: ObjectId, serial: u32, _width: u32, _height: u32) {
        for wallpaper in self.wallpapers.iter() {
            if wallpaper.borrow().has_layer_surface(sender_id) {
                wayland::zwlr_layer_surface_v1::req::ack_configure(
                    &mut self.backend,
                    sender_id,
                    serial,
                )
                .unwrap();
                break;
            }
        }
    }

    fn closed(&mut self, sender_id: ObjectId) {
        if let Some(i) = self
            .wallpapers
            .iter()
            .position(|w| w.borrow().has_layer_surface(sender_id))
        {
            let w = self.wallpapers.remove(i);
            w.borrow_mut().destroy(&mut self.backend);
            self.stop_animations(&[w]);
        }
    }
}

impl wayland::wp_fractional_scale_v1::EvHandler for Daemon {
    fn preferred_scale(&mut self, sender_id: ObjectId, scale: u32) {
        for wallpaper in self.wallpapers.iter() {
            if wallpaper.borrow().has_fractional_scale(sender_id) {
                match NonZeroI32::new(scale as i32) {
                    Some(factor) => {
                        wallpaper.borrow_mut().set_scale(Scale::Fractional(factor));
                        if wallpaper.borrow_mut().commit_surface_changes(
                            &mut self.backend,
                            &mut self.objman,
                            &self.namespace,
                            self.use_cache,
                        ) {
                            self.stop_animations(&[wallpaper.clone()]);
                        }
                    }
                    None => error!("received scale factor of 0 from compositor"),
                }
                break;
            }
        }
    }
}

impl wayland::wp_viewporter::EvHandler for Daemon {}
impl wayland::wp_viewport::EvHandler for Daemon {}
impl wayland::wp_fractional_scale_manager_v1::EvHandler for Daemon {}

impl Drop for Daemon {
    fn drop(&mut self) {
        for wallpaper in self.wallpapers.iter() {
            let mut w = wallpaper.borrow_mut();
            w.destroy(&mut self.backend);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum WaylandObject {
    // standard stuff
    Display,
    Registry,
    Callback,
    Compositor,
    Shm,
    ShmPool,
    Buffer,
    Surface,
    Region,
    Output,

    // layer shell
    LayerShell,
    LayerSurface,

    // Viewporter
    Viewporter,
    Viewport,

    // Fractional Scaling
    FractionalScaler,
    FractionalScale,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // first, get the command line arguments and make the logger
    let cli = cli::Cli::new();
    make_logger(cli.quiet);

    // next, initialize all wayland stuff
    let mut backend = waybackend::connect()?;
    let mut receiver = wire::Receiver::new();
    let mut objman = objman::ObjectManager::<WaylandObject>::new(WaylandObject::Display);
    let registry = objman.create(WaylandObject::Registry);
    let callback = objman.create(WaylandObject::Callback);
    let (mut globals, delete_callback) =
        waybackend::roundtrip(&mut backend, &mut receiver, registry, callback)?;

    if delete_callback {
        objman.remove(callback);
    }

    // macro to help binding the globals
    macro_rules! match_global {
        ($global:ident, $(($interface:ident, $object:path)),*$(,)?) => {
            match $global.interface() {
                $($interface::NAME => $global.bind(&mut backend, registry, &mut objman, $object)?),*,
                _ => (),
            }
        }
    }

    for global in &globals {
        use wayland::*;
        use WaylandObject::*;
        match_global!(
            global,
            (wl_compositor, Compositor),
            (wl_shm, Shm),
            (zwlr_layer_shell_v1, LayerShell),
            (wp_viewporter, Viewporter),
            (wp_fractional_scale_manager_v1, FractionalScaler),
        );
    }
    globals.retain(|global| global.interface() == wayland::wl_output::NAME);

    // create the socket listener and setup the signal handlers
    // this will also return an error if there is an `swww-daemon` instance already
    // running
    let listener = SocketWrapper::new(&cli.namespace)?;
    setup_signals();

    // use the initializer to create the Daemon, then drop it to free up the memory
    let mut daemon = Daemon::new(backend, objman, cli, globals);

    if let Ok(true) = sd_notify::booted() {
        if let Err(e) = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]) {
            error!("Error sending status update to systemd: {e}");
        }
    }

    // dispatch macro
    macro_rules! match_enum_with_interface {
        ($handler:ident, $object:ident, $msg:ident, $(($variant:path, $interface:ident)),*$(,)?) => {
            match $object {
                $($variant => $interface::event(&mut $handler, &mut $msg)?),*,
            }
        }
    }

    // main loop
    while !should_daemon_exit() {
        use rustix::event::{PollFd, PollFlags};
        use wayland::*;
        use WaylandObject::*;

        daemon.backend.flush()?;

        let mut fds = [
            PollFd::new(&daemon.backend.wayland_fd, PollFlags::IN),
            PollFd::new(&listener.fd, PollFlags::IN),
        ];

        // Note: we cannot use rustix::io::retry_on_intr because it makes CTRL-C fail on the
        // terminal
        match rustix::event::poll(&mut fds, daemon.poll_time.as_ref()) {
            Ok(_) => (),
            Err(rustix::io::Errno::INTR | rustix::io::Errno::WOULDBLOCK) => continue,
            Err(e) => return Err(Box::new(e)),
        }

        let wayland_event = !fds[0].revents().is_empty();
        let socket_event = !fds[1].revents().is_empty();

        if wayland_event {
            let mut msg = receiver.recv(&daemon.backend.wayland_fd)?;
            while msg.has_next()? {
                let sender_id = msg.sender_id();
                if sender_id == waybackend::WL_DISPLAY {
                    wl_display::event(&mut daemon, &mut msg)?;
                } else {
                    let sender = daemon
                        .objman
                        .get(sender_id)
                        .expect("received wayland message from unknown object");
                    match_enum_with_interface!(
                        daemon,
                        sender,
                        msg,
                        (Display, wl_display),
                        (Registry, wl_registry),
                        (Callback, wl_callback),
                        (Compositor, wl_compositor),
                        (Shm, wl_shm),
                        (ShmPool, wl_shm_pool),
                        (Buffer, wl_buffer),
                        (Surface, wl_surface),
                        (Region, wl_region),
                        (Output, wl_output),
                        (LayerShell, zwlr_layer_shell_v1),
                        (LayerSurface, zwlr_layer_surface_v1),
                        (Viewporter, wp_viewporter),
                        (Viewport, wp_viewport),
                        (FractionalScaler, wp_fractional_scale_manager_v1),
                        (FractionalScale, wp_fractional_scale_v1),
                    );
                }
            }
        }

        if socket_event {
            // See above note about rustix::retry_on_intr
            match rustix::net::accept(&listener.fd) {
                Ok(stream) => daemon.recv_socket_msg(IpcSocket::new(stream)),
                Err(rustix::io::Errno::INTR | rustix::io::Errno::WOULDBLOCK) => continue,
                Err(e) => return Err(Box::new(e)),
            }
        }

        if daemon.poll_time.is_some() {
            daemon.draw();
        }
    }

    drop(daemon);
    drop(listener);
    info!("Goodbye!");
    Ok(())
}

fn setup_signals() {
    // C data structure, expected to be zeroed out.
    let mut sigaction: libc::sigaction = unsafe { std::mem::zeroed() };
    unsafe { libc::sigemptyset(std::ptr::addr_of_mut!(sigaction.sa_mask)) };

    #[cfg(not(target_os = "aix"))]
    {
        sigaction.sa_sigaction = signal_handler as usize;
    }
    #[cfg(target_os = "aix")]
    {
        sigaction.sa_union.__su_sigaction = handler;
    }

    for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
        let ret =
            unsafe { libc::sigaction(signal, std::ptr::addr_of!(sigaction), std::ptr::null_mut()) };
        if ret != 0 {
            error!("Failed to install signal handler!")
        }
    }
    debug!("Finished setting up signal handlers")
}

/// This is a wrapper that makes sure to delete the socket when it is dropped
struct SocketWrapper {
    fd: OwnedFd,
    namespace: String,
}
impl SocketWrapper {
    fn new(namespace: &str) -> Result<Self, String> {
        let addr = IpcSocket::<Server>::path(namespace);

        if addr.exists() {
            if is_daemon_running(namespace)? {
                return Err(
                    "There is an swww-daemon instance already running on this socket!".to_string(),
                );
            } else {
                warn!(
                    "socket file {} was not deleted when the previous daemon exited",
                    addr.to_string_lossy()
                );
                if let Err(e) = std::fs::remove_file(&addr) {
                    return Err(format!("failed to delete previous socket: {e}"));
                }
            }
        }

        let runtime_dir = match addr.parent() {
            Some(path) => path,
            None => return Err("couldn't find a valid runtime directory".to_owned()),
        };

        if !runtime_dir.exists() {
            match fs::create_dir(runtime_dir) {
                Ok(()) => (),
                Err(e) => return Err(format!("failed to create runtime dir: {e}")),
            }
        }

        let socket = IpcSocket::server(namespace).map_err(|err| err.to_string())?;

        debug!("Created socket in {:?}", addr);
        Ok(Self {
            fd: socket.to_fd(),
            namespace: namespace.to_string(),
        })
    }
}

impl Drop for SocketWrapper {
    fn drop(&mut self) {
        let addr = IpcSocket::<Server>::path(&self.namespace);
        if let Err(e) = fs::remove_file(&addr) {
            error!("Failed to remove socket at {addr:?}: {e}");
        }
        info!("Removed socket at {addr:?}");
    }
}

struct Logger {
    level_filter: LevelFilter,
    start: std::time::Instant,
    is_term: bool,
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level_filter
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let time = self.start.elapsed().as_millis();

            let level = if self.is_term {
                match record.level() {
                    log::Level::Error => "\x1b[31m[ERROR]\x1b[0m",
                    log::Level::Warn => "\x1b[33m[WARN]\x1b[0m ",
                    log::Level::Info => "\x1b[32m[INFO]\x1b[0m ",
                    log::Level::Debug | log::Level::Trace => "\x1b[36m[DEBUG]\x1b[0m",
                }
            } else {
                match record.level() {
                    log::Level::Error => "[ERROR]",
                    log::Level::Warn => "[WARN] ",
                    log::Level::Info => "[INFO] ",
                    log::Level::Debug | log::Level::Trace => "[DEBUG]",
                }
            };

            let msg = record.args();
            let _ = std::io::stderr().write_fmt(format_args!("{time:>10}ms {level} {msg}\n"));
        }
    }

    fn flush(&self) {
        //no op (we do not buffer anything)
    }
}

fn make_logger(quiet: bool) {
    let level_filter = if quiet {
        LevelFilter::Error
    } else {
        LevelFilter::Debug
    };

    log::set_boxed_logger(Box::new(Logger {
        level_filter,
        start: std::time::Instant::now(),
        is_term: std::io::stderr().is_terminal(),
    }))
    .map(|()| log::set_max_level(level_filter))
    .unwrap();
}

pub fn is_daemon_running(namespace: &str) -> Result<bool, String> {
    let sock = match IpcSocket::connect(namespace) {
        Ok(s) => s,
        // likely a connection refused; either way, this is a reliable signal there's no surviving
        // daemon.
        Err(_) => return Ok(false),
    };

    RequestSend::Ping.send(&sock)?;
    let answer = Answer::receive(sock.recv().map_err(|err| err.to_string())?);
    match answer {
        Answer::Ping(_) => Ok(true),
        _ => Err("Daemon did not return Answer::Ping, as expected".to_string()),
    }
}

/// copy-pasted from the `spin_sleep` crate on crates.io
///
/// This will sleep for an amount of time we can roughly expected the OS to still be precise enough
/// for frame timing (125 us, currently).
fn spin_sleep(duration: std::time::Duration) {
    const ACCURACY: std::time::Duration = std::time::Duration::new(0, 125_000);
    let start = std::time::Instant::now();
    if duration > ACCURACY {
        std::thread::sleep(duration - ACCURACY);
    }

    while start.elapsed() < duration {
        std::thread::yield_now();
    }
}
