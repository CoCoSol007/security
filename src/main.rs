use crossbeam_channel::{Receiver, unbounded};
use eframe::egui::{self, ahash::HashMap};
use ffmpeg_next::{self as ffmpeg};
use std::thread;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

struct VideoApp {
    current_url: String,
    running_sender: HashMap<String, crossbeam_channel::Sender<bool>>,
    packet_receiver: Receiver<VideoFrame>,
    texture: Option<egui::TextureHandle>,
    notification_timer: Option<std::time::Instant>,
}

struct VideoStream {
    url: String,
    packet_sender: crossbeam_channel::Sender<VideoFrame>,
    stop_receiver: Receiver<bool>,
    running: bool,
}

struct VideoFrame {
    data: Vec<u8>,
    url: String,
}

const PATHS: [&str; 3] = [
    "rtsp://127.0.0.1:8554/mystream",
    "rtsp://127.0.0.1:8554/mystream2",
    "rtsp://127.0.0.1:8554/mystream3",
];

const NAMES: [&str; 3] = ["Camera 1", "Camera 2", "Camera 3"];

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
        let current_index = PATHS
            .iter()
            .position(|&p| p == self.current_url)
            .unwrap_or(0);
        let next_index = (current_index + 1) % PATHS.len();
        self.switch_stream(PATHS[next_index]);
    }

    fn previous_camera(&mut self) {
        let current_index = PATHS
            .iter()
            .position(|&p| p == self.current_url)
            .unwrap_or(0);
        let next_index = if current_index == 0 {
            PATHS.len() - 1
        } else {
            current_index - 1
        };
        self.switch_stream(PATHS[next_index]);
    }

    fn take_snapshot(&self, frame: &VideoFrame) {
        let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        let num = PATHS
            .iter()
            .position(|&p| p == self.current_url)
            .unwrap_or(0);

        let cam_name = NAMES[num]
            .replace("://", "_")
            .replace("/", "_")
            .replace(".", "_");
        let filename = format!("{}_{}.png", cam_name, timestamp);

        if let Some(img_buffer) =
            image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(1280, 720, frame.data.clone())
        {
            let _ = img_buffer.save(&filename);
        }
    }
}

fn main() -> Result<(), eframe::Error> {
    let (packet_sender, packet_receiver) = unbounded::<VideoFrame>();

    let mut video_app = VideoApp {
        current_url: PATHS[0].to_string(),
        running_sender: HashMap::default(),
        packet_receiver: packet_receiver.clone(),
        texture: None,
        notification_timer: None,
    };

    for path in PATHS.iter() {
        let sender_clone = packet_sender.clone();
        let path_string = path.to_string();
        let (stop_sender, stop_receiver) = unbounded::<bool>();

        thread::spawn(move || {
            let video_stream = VideoStream {
                url: path_string.clone(),
                packet_sender: sender_clone.clone(),
                stop_receiver,
                running: path_string == PATHS[0],
            };
            let _ = run_decoder_managed(video_stream);
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
            ctx.request_repaint();
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
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
            });

        let btn_size = egui::vec2(100.0, 100.0);
        let capture_radius = 34.0;

        egui::Area::new("controls".into())
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -30.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_black_alpha(170))
                    .corner_radius(50.0)
                    .inner_margin(egui::Margin::symmetric(35, 18))
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
                                    self.previous_camera();
                                }
                            }

                            {
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
                                    if let Some(data) = latest_data {
                                        self.take_snapshot(&data);
                                        self.notification_timer = Some(std::time::Instant::now());
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
                                    self.next_camera();
                                }
                            }
                        });
                    });
            });

        let cam_index = PATHS
            .iter()
            .position(|&p| p == self.current_url)
            .unwrap_or(0);
        let cam_name = NAMES[cam_index];

        egui::Area::new("camera_name_overlay".into())
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-10.0, 10.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_black_alpha(200))
                    .inner_margin(16.0)
                    .corner_radius(15.0)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(cam_name)
                                .color(egui::Color32::WHITE)
                                .strong()
                                .size(32.0),
                        );
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

                ctx.request_repaint();
            } else {
                self.notification_timer = None;
            }
        }
    }
}

fn run_decoder_managed(video_stream: VideoStream) -> Result<(), ffmpeg::Error> {
    let mut running = video_stream.running;
    let mut waiting_for_keyframe = true;

    loop {
        let mut ictx = match ffmpeg::format::input(&video_stream.url) {
            Ok(ctx) => ctx,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        };

        let input = ictx.streams().best(ffmpeg::media::Type::Video).unwrap();
        let video_index = input.index();
        let context = ffmpeg::codec::context::Context::from_parameters(input.parameters())?;
        let mut decoder = context.decoder().video()?;
        let mut scaler = ffmpeg::software::scaling::context::Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            ffmpeg::format::Pixel::RGBA,
            WIDTH,
            HEIGHT,
            ffmpeg::software::scaling::flag::Flags::BILINEAR,
        )?;

        let mut frame = ffmpeg::util::frame::video::Video::empty();
        let mut frame_rgba = ffmpeg::util::frame::video::Video::empty();

        for (stream, packet) in ictx.packets() {
            if let Ok(value) = video_stream.stop_receiver.try_recv() {
                if value && !running {
                    waiting_for_keyframe = true;
                }
                running = value;
            }

            if stream.index() == video_index && running {
                if waiting_for_keyframe {
                    if packet.is_key() {
                        waiting_for_keyframe = false;
                    } else {
                        continue;
                    }
                }

                let _ = decoder.send_packet(&packet);
                while decoder.receive_frame(&mut frame).is_ok() {
                    let _ = scaler.run(&frame, &mut frame_rgba);
                    if video_stream
                        .packet_sender
                        .send(VideoFrame {
                            data: frame_rgba.data(0).to_vec(),
                            url: video_stream.url.clone(),
                        })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}
