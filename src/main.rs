use crossbeam_channel::{Receiver, unbounded};
use eframe::egui::RichText;
use eframe::egui::{self, ahash::HashMap};
use ffmpeg_next::Dictionary;
use ffmpeg_next::{self as ffmpeg};
use serde::Deserialize;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::time::sleep;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const SLEEP_TIME: u64 = 5; // secondes

struct VideoApp {
    config: RootConfig,
    current_url: String,
    running_sender: HashMap<String, crossbeam_channel::Sender<bool>>,
    packet_receiver: Receiver<VideoFrame>,
    texture: Option<egui::TextureHandle>,
    notification_timer: Option<std::time::Instant>,
    show_gallery: bool,
    gallery_images: Vec<std::path::PathBuf>,
    gallery_index: usize,
    gallery_texture: Option<egui::TextureHandle>,
    last_activity: std::time::Instant,
    is_down: bool,
    wakeup_rx: Receiver<()>,
}

struct VideoFrame {
    data: Arc<Vec<u8>>,
    url: String,
}

#[derive(Deserialize, Debug)]
struct Config {
    has_to_wait_for_keyframe: bool,
    capture_path: String,
    cursor_visible: bool,
    use_tcp_for_rtsp: bool,
}

#[derive(Deserialize, Debug)]
struct Camera {
    name: String,
    url: String,
}

#[derive(Deserialize, Debug)]
struct DoorbellConfig {
    bell_ip: String,
    mdp: String,
}

#[derive(Deserialize, Debug)]
struct RootConfig {
    config: Config,
    bell: DoorbellConfig,
    camera: Vec<Camera>,
}

impl RootConfig {
    fn get_camera_urls(&self) -> Vec<String> {
        self.camera.iter().map(|cam| cam.url.clone()).collect()
    }

    fn get_camera_names(&self) -> Vec<String> {
        self.camera.iter().map(|cam| cam.name.clone()).collect()
    }
}

impl VideoApp {
    fn switch_stream(&mut self, new_url: &str) {
        if let Some(sender) = self.running_sender.get(&self.current_url) {
            let _ = sender.send(false);
        }

        if let Some(sender) = self.running_sender.get(new_url) {
            let _ = sender.send(true);
        }

        self.current_url = new_url.to_string();
        self.texture = None;
    }

    fn next_camera(&mut self) {
        let current_index = self
            .config
            .get_camera_urls()
            .iter()
            .position(|p| p == &self.current_url)
            .unwrap_or(0);
        let next_index = (current_index + 1) % self.config.get_camera_urls().len();
        self.switch_stream(&self.config.get_camera_urls()[next_index]);
    }

    fn previous_camera(&mut self) {
        let current_index = self
            .config
            .get_camera_urls()
            .iter()
            .position(|p| p == &self.current_url)
            .unwrap_or(0);
        let next_index = if current_index == 0 {
            self.config.get_camera_urls().len() - 1
        } else {
            current_index - 1
        };
        self.switch_stream(&self.config.get_camera_urls()[next_index]);
    }

    fn take_snapshot(&self, frame: &VideoFrame) {
        let data_arc = Arc::clone(&frame.data);
        let path = self.config.config.capture_path.clone();
        let cam_name = self
            .config
            .camera
            .iter()
            .find(|c| c.url == frame.url)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "unknown".into());

        thread::spawn(move || {
            let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
            let filename = format!("{}/{}_{}.png", path, timestamp, cam_name.replace(" ", "_"));

            if let Some(buf) = image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(
                WIDTH,
                HEIGHT,
                (&*data_arc).clone(),
            ) {
                let _ = buf.save(filename);
            }
        });
    }

    fn open_gallery(&mut self) {
        self.gallery_images = match std::fs::read_dir(&self.config.config.capture_path) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|d| d.path()))
                .filter(|p| {
                    if let Some(ext) = p.extension() {
                        match ext.to_string_lossy().to_lowercase().as_str() {
                            "png" | "jpg" | "jpeg" => true,
                            _ => false,
                        }
                    } else {
                        false
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        };

        self.gallery_images.sort();
        self.gallery_images.reverse();
        self.gallery_index = 0;
        self.show_gallery = true;
        self.gallery_texture = None;
    }

    fn load_gallery_texture(&mut self, ctx: &egui::Context) {
        if self.gallery_images.is_empty() {
            self.gallery_texture = None;
            return;
        }

        if let Some(path) = self.gallery_images.get(self.gallery_index) {
            if let Ok(img) = image::open(path) {
                let img = img.to_rgba8();
                let size = [img.width() as usize, img.height() as usize];
                let pixels = img.into_raw();
                let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
                let id = format!("gallery:{}", path.display());
                self.gallery_texture =
                    Some(ctx.load_texture(&id, color_image, egui::TextureOptions::LINEAR));
            } else {
                self.gallery_texture = None;
            }
        }
    }

    fn gallery_next(&mut self) {
        if self.gallery_images.is_empty() {
            return;
        }
        self.gallery_index = (self.gallery_index + 1) % self.gallery_images.len();
        self.gallery_texture = None;
    }

    fn gallery_previous(&mut self) {
        if self.gallery_images.is_empty() {
            return;
        }
        if self.gallery_index == 0 {
            self.gallery_index = self.gallery_images.len() - 1;
        } else {
            self.gallery_index -= 1;
        }
        self.gallery_texture = None;
    }

    fn close_gallery(&mut self) {
        self.show_gallery = false;
        self.gallery_texture = None;
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();
    ffmpeg::init().expect("FFmpeg init failed");

    let content = std::fs::read_to_string("config.toml").unwrap();
    let config: RootConfig = toml::from_str(&content).unwrap();

    let (wakeup_tx, wakeup_rx) = unbounded();

    let mut monitor = DoorbellMonitor::new(&config.bell.bell_ip, &config.bell.mdp, wakeup_tx);
    tokio::spawn(async move {
        println!("Démarrage du moniteur de sonnette...");
        monitor.run().await;
    });

    let (packet_sender, packet_receiver) = unbounded();
    let mut running_sender = HashMap::default();

    for cam in &config.camera {
        let (stop_tx, stop_rx) = unbounded();
        let url = cam.url.clone();
        let p_sender = packet_sender.clone();
        let wait_key = config.config.has_to_wait_for_keyframe;
        let use_tcp = config.config.use_tcp_for_rtsp;
        let is_active = url == config.camera[0].url;

        thread::spawn(move || {
            let _ = run_decoder_loop(url, p_sender, stop_rx, wait_key, use_tcp, is_active);
        });
        running_sender.insert(cam.url.clone(), stop_tx);
    }

    let app = VideoApp {
        current_url: config.camera[0].url.clone(),
        config,
        running_sender,
        packet_receiver,
        texture: None,
        notification_timer: None,
        show_gallery: false,
        gallery_images: Vec::new(),
        gallery_index: 0,
        gallery_texture: None,
        last_activity: Instant::now(),
        is_down: false,
        wakeup_rx,
    };

    let _ = eframe::run_native(
        "CCTV Optimizer",
        eframe::NativeOptions::default(),
        Box::new(|_| Ok(Box::new(app))),
    );
}

impl eframe::App for VideoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.input(|i| {
            let should_quit = i.events.iter().any(|e| match e {
                egui::Event::Key { key, pressed, .. } => *pressed && *key == egui::Key::Q,
                _ => false,
            });

            if should_quit {
                std::process::exit(0);
            }
        });

        ctx.output_mut(|o| {
            o.cursor_icon = if self.config.config.cursor_visible {
                egui::CursorIcon::Default
            } else {
                egui::CursorIcon::None
            };
        });

        let has_activity = ctx.input(|i| {
            !i.events.is_empty() || i.pointer.any_click() || i.pointer.delta().length() > 0.0
        });

        if let Ok(_) = self.wakeup_rx.try_recv() {
            println!("App réveillée par la sonnette !");
            self.last_activity = Instant::now();
            if self.is_down {
                if let Some(sender) = self.running_sender.get(&self.current_url) {
                    let _ = sender.send(true);
                    self.is_down = false;
                }
            }
        }

        if has_activity {
            if self.last_activity.elapsed().as_secs() >= SLEEP_TIME {
                if let Some(sender) = self.running_sender.get(&self.current_url) {
                    let _ = sender.send(true);
                    self.is_down = false;
                }
            }
            self.last_activity = std::time::Instant::now();
        }

        if self.last_activity.elapsed().as_secs() >= SLEEP_TIME && !self.is_down {
            for sender in self.running_sender.values() {
                let _ = sender.send(false);
                self.is_down = true;
                //  self.texture = None;
            }
        }

        let mut latest_data = None;
        while let Ok(data) = self.packet_receiver.try_recv() {
            if self.current_url != data.url {
                continue;
            }
            latest_data = Some(data);
        }

        if let Some(frame) = latest_data.as_ref() {
            let size = [WIDTH as usize, HEIGHT as usize];
            let ci = egui::ColorImage::from_rgb(size, &frame.data);

            if let Some(t) = &mut self.texture {
                t.set(ci, egui::TextureOptions::LINEAR);
            } else {
                self.texture = Some(ctx.load_texture("video", ci, egui::TextureOptions::LINEAR));
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                if self.show_gallery {
                    if self.gallery_texture.is_none() {
                        self.load_gallery_texture(ctx);
                    }

                    if let Some(texture) = &self.gallery_texture {
                        let available = ui.available_size();
                        let image_size = texture.size_vec2();
                        let image_ratio = image_size.x / image_size.y;
                        let final_size = if (available.x / available.y) > image_ratio {
                            egui::vec2(available.y * image_ratio, available.y)
                        } else {
                            egui::vec2(available.x, available.x / image_ratio)
                        };

                        ui.centered_and_justified(|ui| {
                            ui.add(egui::Image::new(texture).fit_to_exact_size(final_size));
                        });
                    } else {
                        ui.centered_and_justified(|ui| {
                            ui.label(RichText::new("Aucune image dans le dossier...").size(32.));
                        });
                    }
                } else {
                    if let Some(texture) = &self.texture {
                        let available = ui.available_size();
                        let image_size = texture.size_vec2();
                        let image_ratio = image_size.x / image_size.y;
                        let final_size = if (available.x / available.y) > image_ratio {
                            egui::vec2(available.y * image_ratio, available.y)
                        } else {
                            egui::vec2(available.x, available.x / image_ratio)
                        };

                        ui.centered_and_justified(|ui| {
                            ui.add(egui::Image::new(texture).fit_to_exact_size(final_size));
                        });
                    } else {
                        ui.centered_and_justified(|ui| {
                            ui.add(egui::Spinner::new().size(64.0));
                        });
                    }
                }
            });

        let btn_size = egui::vec2(130.0, 130.0);
        let capture_radius = 44.0;

        egui::Area::new("controls".into())
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -10.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_black_alpha(170))
                    .corner_radius(50.0)
                    .inner_margin(egui::Margin::symmetric(5, 2))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 40.0;
                            {
                                let (rect, resp) =
                                    ui.allocate_exact_size(btn_size, egui::Sense::click());

                                if resp.hovered() {
                                    ui.painter().circle_filled(
                                        rect.center(),
                                        50.0,
                                        egui::Color32::from_white_alpha(20),
                                    );
                                }

                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "◀",
                                    egui::FontId::proportional(64.0),
                                    egui::Color32::WHITE,
                                );

                                if resp.clicked() {
                                    if self.show_gallery {
                                        self.gallery_previous();
                                        self.load_gallery_texture(ctx);
                                    } else {
                                        self.previous_camera();
                                    }
                                }
                            }

                            if !self.show_gallery {
                                let (rect, resp) =
                                    ui.allocate_exact_size(btn_size, egui::Sense::click());

                                ui.painter().circle_filled(
                                    rect.center() + egui::vec2(0.0, 4.0),
                                    capture_radius + 4.0,
                                    egui::Color32::from_black_alpha(90),
                                );

                                let color = if resp.hovered() {
                                    egui::Color32::from_rgb(230, 60, 60)
                                } else {
                                    egui::Color32::from_rgb(200, 30, 30)
                                };

                                ui.painter()
                                    .circle_filled(rect.center(), capture_radius, color);

                                ui.painter().circle_stroke(
                                    rect.center(),
                                    capture_radius - 10.0,
                                    egui::Stroke::new(3.0, egui::Color32::WHITE),
                                );

                                if resp.clicked() {
                                    if !self.show_gallery {
                                        if let Some(data) = latest_data {
                                            self.take_snapshot(&data);
                                            self.notification_timer =
                                                Some(std::time::Instant::now());
                                        }
                                    }
                                }
                            }
                            {
                                let (rect, resp) =
                                    ui.allocate_exact_size(btn_size, egui::Sense::click());

                                if resp.hovered() {
                                    ui.painter().circle_filled(
                                        rect.center(),
                                        50.0,
                                        egui::Color32::from_white_alpha(20),
                                    );
                                }

                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    if self.show_gallery { "❌" } else { "🖼" },
                                    egui::FontId::proportional(48.0),
                                    egui::Color32::WHITE,
                                );

                                if resp.clicked() {
                                    if self.show_gallery {
                                        self.close_gallery();
                                    } else {
                                        self.open_gallery();
                                        self.load_gallery_texture(ctx);
                                    }
                                }
                            }

                            {
                                let (rect, resp) =
                                    ui.allocate_exact_size(btn_size, egui::Sense::click());

                                if resp.hovered() {
                                    ui.painter().circle_filled(
                                        rect.center(),
                                        50.0,
                                        egui::Color32::from_white_alpha(20),
                                    );
                                }

                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "▶",
                                    egui::FontId::proportional(64.0),
                                    egui::Color32::WHITE,
                                );

                                if resp.clicked() {
                                    if self.show_gallery {
                                        self.gallery_next();
                                        self.load_gallery_texture(ctx);
                                    } else {
                                        self.next_camera();
                                    }
                                }
                            }
                        });
                    });
            });

        if self.show_gallery {
            return;
        }

        let cam_index = self
            .config
            .get_camera_urls()
            .iter()
            .position(|p| p == &self.current_url)
            .unwrap_or(0);
        let cam_name = self.config.get_camera_names()[cam_index].clone();

        egui::Area::new("camera_name_overlay".into())
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 10.0))
            .pivot(egui::Align2::CENTER_TOP)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_black_alpha(200))
                    .inner_margin(16.0)
                    .corner_radius(15.0)
                    .show(ui, |ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        ui.set_min_width(0.0);
                        ui.label(
                            egui::RichText::new(cam_name)
                                .color(egui::Color32::WHITE)
                                .strong()
                                .size(32.0),
                        )
                    });
            });

        if let Some(start) = self.notification_timer {
            let elapsed = start.elapsed().as_secs_f32();
            let flash_duration = 0.15;

            if elapsed < flash_duration {
                let alpha = 1.0 - (elapsed / flash_duration);
                let alpha = (alpha * 220.0) as u8;

                let rect = ctx.viewport_rect();

                ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("flash_layer"),
                ))
                .rect_filled(rect, 0.0, egui::Color32::from_white_alpha(alpha));
            } else {
                self.notification_timer = None;
            }
        }
        ctx.request_repaint();
    }
}

fn run_decoder_loop(
    url: String,
    sender: crossbeam_channel::Sender<VideoFrame>,
    stop_rx: Receiver<bool>,
    wait_key: bool,
    use_tcp: bool,
    mut active: bool,
) -> Result<(), ffmpeg::Error> {
    loop {
        let mut opts = Dictionary::new();
        if use_tcp {
            opts.set("rtsp_transport", "tcp");
        }

        if let Ok(mut ictx) = ffmpeg::format::input_with_dictionary(&url, opts) {
            let input = ictx.streams().best(ffmpeg::media::Type::Video).unwrap();
            let idx = input.index();
            let mut decoder_ctx =
                ffmpeg::codec::context::Context::from_parameters(input.parameters())?;
            decoder_ctx.set_threading(ffmpeg::codec::threading::Config {
                kind: ffmpeg::codec::threading::Type::Frame,
                count: 0,
            });
            let mut decoder = decoder_ctx.decoder().video()?;

            let mut scaler = ffmpeg::software::scaling::context::Context::get(
                decoder.format(),
                decoder.width(),
                decoder.height(),
                ffmpeg::format::Pixel::RGB24,
                WIDTH,
                HEIGHT,
                ffmpeg::software::scaling::flag::Flags::POINT,
            )?;

            let mut frame = ffmpeg::util::frame::video::Video::empty();
            let mut frame_rgb = ffmpeg::util::frame::video::Video::empty();
            let mut waiting = wait_key;
            let mut packed = vec![0u8; WIDTH as usize * HEIGHT as usize * 3];

            for (stream, packet) in ictx.packets() {
                if let Ok(state) = stop_rx.try_recv() {
                    active = state;
                    if active {
                        waiting = wait_key;
                    }
                }

                if stream.index() == idx {
                    if !active {
                        // On vide le buffer réseau pour rester "en direct"
                        continue;
                    }

                    if waiting && !packet.is_key() {
                        continue;
                    }
                    waiting = false;

                    if stream.index() == idx {
                        if waiting && !packet.is_key() {
                            continue;
                        }
                        waiting = false;

                        if decoder.send_packet(&packet).is_ok() {
                            while decoder.receive_frame(&mut frame).is_ok() {
                                let _ = scaler.run(&frame, &mut frame_rgb);

                                let width = frame_rgb.width() as usize;
                                let height = frame_rgb.height() as usize;
                                let _ = scaler.run(&frame, &mut frame_rgb);
                                let src = frame_rgb.data(0);
                                let stride = frame_rgb.stride(0);

                                for y in 0..HEIGHT as usize {
                                    let src_start = y * stride;
                                    let dst_start = y * WIDTH as usize * 3;
                                    packed[dst_start..dst_start + WIDTH as usize * 3]
                                        .copy_from_slice(
                                            &src[src_start..src_start + WIDTH as usize * 3],
                                        );
                                }

                                for y in 0..height {
                                    let src_start = y * stride;
                                    let dst_start = y * width * 3;
                                    packed[dst_start..dst_start + width * 3]
                                        .copy_from_slice(&src[src_start..src_start + width * 3]);
                                }

                                // On envoie un Arc pour éviter le .to_vec()
                                let data = Arc::new(packed.clone());
                                let _ = sender.try_send(VideoFrame {
                                    data,
                                    url: url.clone(),
                                });
                            }
                        }
                    }
                }
            }
            thread::sleep(Duration::from_secs(5)); // Retry connexion
        }
    }
}

// --- LE MAPPAGE SERDE COMPLET ---
#[derive(Debug, Deserialize)]
struct ReolinkResponse {
    value: ReolinkValue,
}

#[derive(Debug, Deserialize)]
struct ReolinkValue {
    ai: Option<AiEvents>,
    md: Option<AlarmStatus>,
    visitor: Option<AlarmStatus>,
}

#[derive(Debug, Deserialize)]
struct AiEvents {
    people: Option<AlarmStatus>,
}

#[derive(Debug, Deserialize)]
struct AlarmStatus {
    #[serde(default)]
    alarm_state: i32,
}

struct DoorbellMonitor {
    ip: String,
    mdp: String,
    wakeup_tx: crossbeam_channel::Sender<()>,
}

impl DoorbellMonitor {
    fn new(ip: &str, mdp: &str, wakeup_tx: crossbeam_channel::Sender<()>) -> Self {
        Self {
            ip: ip.to_string(),
            mdp: mdp.to_string(),
            wakeup_tx,
        }
    }

    async fn run(&mut self) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();

        loop {
            println!("--- Surveillance active sur {} ---", self.ip);
            if let Err(e) = self.listen_loop(&client).await {
                println!("Erreur de connexion : {}. Reconnexion dans 5s...", e);
            }
            sleep(Duration::from_secs(5)).await;
        }
    }

    async fn listen_loop(
        &mut self,
        client: &reqwest::Client,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let url = format!(
            "http://{}/cgi-bin/api.cgi?cmd=GetEvents&user=admin&password={}",
            self.ip, self.mdp
        );

        loop {
            match client.get(&url).send().await {
                Ok(res) => {
                    // On parse avec sécurité
                    let a = res.text().await?;
                    let res: Result<Vec<ReolinkResponse>, serde_json::Error> =
                        serde_json::from_str(&a);
                    match res {
                        Ok(data_list) => {
                            if let Some(event) = data_list.first() {
                                let bouton = event
                                    .value
                                    .visitor
                                    .as_ref()
                                    .map(|s| s.alarm_state)
                                    .unwrap_or(0);
                                let humain = event
                                    .value
                                    .ai
                                    .as_ref()
                                    .and_then(|a| a.people.as_ref())
                                    .map(|p| p.alarm_state)
                                    .unwrap_or(0);

                                if bouton == 1 {
                                    println!("Sonnette pressée ou détection humaine !");
                                    let _ = self.wakeup_tx.send(());
                                    Command::new("swaymsg").arg("output * dpms on").spawn().ok();
                                }
                            }
                        }
                        Err(e) => println!("JSON incomplet ou différent : {}", e),
                    }
                }
                Err(e) => println!("Problème réseau : {}", e),
            }
            sleep(Duration::from_millis(300)).await;
        }
    }
}
