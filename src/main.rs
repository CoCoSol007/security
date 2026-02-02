use crossbeam_channel::{Receiver, unbounded};
use eframe::egui::RichText;
use eframe::egui::{self, ahash::HashMap};
use serde::Deserialize;
use std::thread;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

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
}

struct VideoStream {
    url: String,
    packet_sender: crossbeam_channel::Sender<VideoFrame>,
    stop_receiver: Receiver<bool>,
}

struct VideoFrame {
    data: Vec<u8>,
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
struct RootConfig {
    config: Config,
    camera: Vec<Camera>,
}

impl RootConfig {
    fn get_camera_urls(&self) -> Vec<String> {
        self.camera.iter().map(|cam| cam.url.clone()).collect()
    }

    fn get_camera_names(&self) -> Vec<String> {
        self.camera.iter().map(|cam| cam.name.clone()).collect()
    }

    fn get_first_camera_url(&self) -> Option<String> {
        self.camera.first().map(|cam| cam.url.clone())
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
        let data = frame.data.clone();
        let capture_path = self.config.config.capture_path.clone();
        let current_url = self.current_url.clone();

        let num = self
            .config
            .get_camera_urls()
            .iter()
            .position(|p| p == &current_url)
            .unwrap_or(0);
        let raw_cam_name = self.config.get_camera_names()[num].clone();

        thread::spawn(move || {
            let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();

            let cam_name = raw_cam_name
                .replace("://", "_")
                .replace("/", "_")
                .replace(".", "_");

            let filename = format!("{}/{}_{}.png", capture_path, timestamp, cam_name);

            if let Some(img_buffer) =
                image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(1280, 720, data)
            {
                if let Err(e) = img_buffer.save(&filename) {
                    eprintln!("Erreur lors de la sauvegarde de l'image : {}", e);
                }
            } else {
                eprintln!("Échec de la création du buffer d'image");
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

fn main() -> Result<(), eframe::Error> {
    let content = std::fs::read_to_string("config.toml").expect("Impossible de lire le fichier");
    let parsed: RootConfig = toml::from_str(&content).expect("Impossible de parser le fichier");

    let (packet_sender, packet_receiver) = unbounded::<VideoFrame>();

    let mut video_app = VideoApp {
        current_url: parsed.get_first_camera_url().unwrap_or_default(),
        running_sender: HashMap::default(),
        packet_receiver: packet_receiver.clone(),
        texture: None,
        notification_timer: None,
        config: parsed,
        show_gallery: false,
        gallery_images: Vec::new(),
        gallery_index: 0,
        gallery_texture: None,
        last_activity: std::time::Instant::now(),
    };

    let _ = video_app.config.config.has_to_wait_for_keyframe;

    for path in video_app.config.get_camera_urls().iter() {
        let sender_clone = packet_sender.clone();
        let path_string = path.to_string();
        let (stop_sender, stop_receiver) = unbounded::<bool>();
        thread::spawn(move || {
            let video_stream = VideoStream {
                url: path_string.clone(),
                packet_sender: sender_clone.clone(),
                stop_receiver,
            };
            let _ = run_decoder_managed(video_stream, video_app.config.config.use_tcp_for_rtsp);
        });

        video_app
            .running_sender
            .insert(path.to_string(), stop_sender);
    }

    let options = eframe::NativeOptions {
        ..Default::default()
    };

    eframe::run_native(
        "Security Camera Viewer",
        options,
        Box::new(|_cc| Ok(Box::new(video_app))),
    )
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

        if has_activity {
            if self.last_activity.elapsed().as_secs() >= 15 {
                if let Some(sender) = self.running_sender.get(&self.current_url) {
                    let _ = sender.send(true);
                }
            }
            self.last_activity = std::time::Instant::now();
        }

        if self.last_activity.elapsed().as_secs() >= 15 {
            for sender in self.running_sender.values() {
                let _ = sender.send(false);
                self.texture = None;
            }
        }

        let mut latest_data = None;
        while let Ok(data) = self.packet_receiver.try_recv() {
            if self.current_url != data.url {
                continue;
            }
            latest_data = Some(data);
        }

        if let Some(data) = latest_data.as_ref() {
            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [WIDTH as usize, HEIGHT as usize],
                &data.data,
            );
            self.texture =
                Some(ctx.load_texture("video_frame", color_image, egui::TextureOptions::LINEAR));
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

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;

fn run_decoder_managed(
    video_stream: VideoStream,
    use_tcp_for_rtsp: bool,
) -> Result<(), anyhow::Error> {
    gst::init()?;

    let protocols = if use_tcp_for_rtsp { "tcp" } else { "udp" };

    let pipeline_str = format!(
        "rtspsrc location=\"{}\" protocols={} latency=0 ! \
     rtph265depay ! h265parse ! avdec_h265 ! \
     videoconvert ! videoscale ! \
     video/x-raw,format=RGBA,width={},height={} ! \
     appsink name=sink drop=true max-buffers=1",
        video_stream.url, protocols, WIDTH, HEIGHT
    );

    let pipeline = gst::parse::launch(&pipeline_str)?;
    let pipeline = pipeline.dynamic_cast::<gst::Pipeline>().unwrap();

    let appsink = pipeline
        .by_name("sink")
        .expect("Sink non trouvé")
        .dynamic_cast::<gst_app::AppSink>()
        .expect("L'élément n'est pas un AppSink");

    let url_clone = video_stream.url.clone();
    let sender_clone = video_stream.packet_sender.clone();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                let _ = sender_clone.try_send(VideoFrame {
                    data: map.to_vec(),
                    url: url_clone.clone(),
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing)?;

    let bus = pipeline.bus().unwrap();
    loop {
        if let Ok(stop) = video_stream.stop_receiver.try_recv() {
            if stop {
                pipeline.seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                    gst::ClockTime::ZERO,
                )?;
                pipeline.set_state(gst::State::Playing)?;
            } else {
                pipeline.set_state(gst::State::Paused)?;
            }
        }

        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    eprintln!("Erreur GStreamer ({}): {}", video_stream.url, err.error());
                    break;
                }
                gst::MessageView::Eos(_) => break,
                _ => (),
            }
        }
    }

    let _ = pipeline.set_state(gst::State::Null);
    Ok(())
}
