#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::{egui, App};
use std::process::{Command, Stdio, Child};
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::thread;
use std::path::Path;
use std::env;
use egui::{FontDefinitions, FontFamily};

#[cfg(target_os="windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
mod winctx {
    use std::io;
    use std::path::PathBuf;
    use winreg::enums::*;
    use winreg::RegKey;

    pub fn add_context_menu(app_path: &str) -> io::Result<()> {
        let hkcr = RegKey::predef(HKEY_CLASSES_ROOT);
        let (shell, _) = hkcr.create_subkey(r"*\\shell\\FFmpeg_Transcoder")?;
        shell.set_value("", &"使用 FFmpeg 转换")?;
        let (cmd, _) = shell.create_subkey("command")?;
        let command = format!("\"{}\" \"%1\"", app_path);
        cmd.set_value("", &command)?;
        Ok(())
    }

    pub fn remove_context_menu() -> io::Result<()> {
        let hkcr = RegKey::predef(HKEY_CLASSES_ROOT);
        hkcr.delete_subkey_all(r"*\\shell\\FFmpeg_Transcoder")?;
        Ok(())
    }

    pub fn get_app_path() -> PathBuf {
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ffmpeg_gui.exe"))
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    // 尝试加载系统常见中文字体
    #[cfg(target_os = "windows")]
    {
        let yahei = r"C:\Windows\Fonts\msyh.ttc"; // Microsoft YaHei
        if Path::new(yahei).exists() {
            use egui::FontData;

            fonts.font_data.insert(
                "yahei".to_owned(),
                FontData::from_owned(std::fs::read(yahei).unwrap())
            );
            fonts.families.entry(FontFamily::Proportional).or_default()
                .insert(0, "yahei".to_owned());
            fonts.families.entry(FontFamily::Monospace).or_default()
                .insert(0, "yahei".to_owned());
        }
    }
    ctx.set_fonts(fonts);
}

struct FFUIApp {
    file: String,
    format: String,
    gpu: String,
    progress: Arc<Mutex<f32>>,
    running: Arc<Mutex<bool>>,
    log_text: Arc<Mutex<String>>,
    completed: Arc<Mutex<bool>>,
    child_process: Arc<Mutex<Option<Child>>>,
    stop_flag: Arc<AtomicBool>,
}

impl FFUIApp {
    fn get_duration(input: &str) -> f64 {
        let output = Command::new("ffprobe")
            .args(&[
                "-v", "error",
                "-show_entries", "format=duration",
                "-of", "default=noprint_wrappers=1:nokey=1",
                input
            ])
            .output()
            .expect("无法执行 ffprobe");
        String::from_utf8_lossy(&output.stdout).trim().parse::<f64>().unwrap_or(0.0)
    }

    fn get_media_info(input: &str) -> String {
        let output = Command::new("ffprobe")
            .args(&["-i", input, "-hide_banner"])
            .output()
            .unwrap_or_else(|_| panic!("无法执行 ffprobe"));
        String::from_utf8_lossy(&output.stderr).to_string()
    }
}

impl App for FFUIApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        use egui::{ComboBox, ScrollArea, ProgressBar};

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(format!("输入文件: {}", self.file));

            ComboBox::from_label("目标格式")
                .selected_text(&self.format)
                .show_ui(ui, |ui| {
                    for fmt in &["mp4","avi","mkv","mov","flv","wmv","mp3","aac","wav","ogg"] {
                        ui.selectable_value(&mut self.format, fmt.to_string(), *fmt);
                    }
                });

            ComboBox::from_label("处理设备")
                .selected_text(&self.gpu)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.gpu, "CPU".to_string(), "CPU");
                    ui.selectable_value(&mut self.gpu, "NVIDIA".to_string(), "NVIDIA GPU");
                    ui.selectable_value(&mut self.gpu, "Intel".to_string(), "Intel GPU");
                    ui.selectable_value(&mut self.gpu, "AMD".to_string(), "AMD GPU");
                });

            ui.horizontal(|ui| {
                if ui.button("开始转换").clicked() && !*self.running.lock().unwrap() {
                    let input = self.file.clone();
                    let output = format!("{}.{}", input, self.format);
                    let progress = self.progress.clone();
                    let running = self.running.clone();
                    let log_text = self.log_text.clone();
                    let completed = self.completed.clone();
                    let child_arc = self.child_process.clone();
                    let stop_flag = self.stop_flag.clone();
                    let gpu_option = self.gpu.clone();
                    let _format = self.format.clone();

                    *running.lock().unwrap() = true;
                    *completed.lock().unwrap() = false;
                    *log_text.lock().unwrap() = FFUIApp::get_media_info(&input);
                    *progress.lock().unwrap() = 0.0;
                    stop_flag.store(false, Ordering::SeqCst);

                    thread::spawn(move || {
                        let duration = FFUIApp::get_duration(&input);

                        let codec = match gpu_option.as_str() {
                            "NVIDIA" => "h264_nvenc",
                            "Intel" => "h264_qsv",
                            "AMD" => "h264_amf",
                            _ => "libx264",
                        };

                        let mut cmd = Command::new("ffmpeg");
                        if gpu_option != "CPU" {
                            match gpu_option.as_str() {
                                "NVIDIA" => { cmd.args(&["-hwaccel","cuda"]); },
                                "Intel" => { cmd.args(&["-hwaccel","qsv"]); },
                                "AMD" => { cmd.args(&["-hwaccel","dxva2"]); },
                                _ => {},
                            }
                        }

                        cmd.args(&[
                            "-y",
                            "-i", &input,
                            "-c:v", codec,
                            &output,
                            "-progress", "pipe:1",
                            "-nostats"
                        ])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null());

                        #[cfg(target_os="windows")]
                        { cmd.creation_flags(0x08000000); }

                        let child = cmd.spawn().expect("无法启动 ffmpeg");
                        *child_arc.lock().unwrap() = Some(child);

                        if let Some(stdout) = child_arc.lock().unwrap().as_mut().unwrap().stdout.take() {
                            let reader = BufReader::new(stdout);
                            for line in reader.lines().flatten() {
                                if stop_flag.load(Ordering::SeqCst) { break; }
                                if line.starts_with("out_time_ms=") && duration > 0.0 {
                                    if let Ok(ms) = line["out_time_ms=".len()..].parse::<f64>() {
                                        *progress.lock().unwrap() = ((ms / (duration*1_000_000.0)) * 100.0) as f32;
                                    }
                                }
                            }
                        }

                        if stop_flag.load(Ordering::SeqCst) {
                            if let Some(mut c) = child_arc.lock().unwrap().take() {
                                let _ = c.kill();
                            }
                            let mut log = log_text.lock().unwrap();
                            log.push_str("\n=== 已中断 ===\n");
                            *progress.lock().unwrap() = 0.0;
                        } else {
                            let _ = child_arc.lock().unwrap().take().unwrap().wait();
                            let path = Path::new(&output);
                            if !path.exists() || path.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
                                let mut log = log_text.lock().unwrap();
                                log.push_str("\n=== 转换失败：输出文件为空 ===\n");
                                *completed.lock().unwrap() = false;
                                *progress.lock().unwrap() = 0.0;
                            } else {
                                *completed.lock().unwrap() = true;
                                *progress.lock().unwrap() = 100.0;
                                let mut log = log_text.lock().unwrap();
                                log.push_str("\n=== 转换完成 ===\n");
                            }
                        }
                        *running.lock().unwrap() = false;
                    });
                }

                if ui.button("中断").clicked() {
                    self.stop_flag.store(true, Ordering::SeqCst);
                }
            });

            let p = *self.progress.lock().unwrap();
            ui.add(ProgressBar::new(p / 100.0).show_percentage());

            ScrollArea::vertical().show(ui, |ui| {
                let log = self.log_text.lock().unwrap();
                ui.monospace(log.as_str());
            });

            if *self.completed.lock().unwrap() {
                ui.label("✅ 转换完成！");
            }
        });

        ctx.request_repaint();
    }
}

struct ContextMenuApp {
    log: String,
}

impl App for ContextMenuApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("右键菜单");

            #[cfg(target_os = "windows")]
            {
                if ui.button("添加到右键菜单").clicked() {
                    let path = winctx::get_app_path();
                    match winctx::add_context_menu(path.to_str().unwrap()) {
                        Ok(_) => self.log = "✅ 完成".to_string(),
                        Err(e) => self.log = format!("❌ 失败: {}", e),
                    }
                }
                if ui.button("从右键菜单移除").clicked() {
                    match winctx::remove_context_menu() {
                        Ok(_) => self.log = "✅ 完成".to_string(),
                        Err(e) => self.log = format!("❌ 失败: {}", e),
                    }
                }
            }

            ui.separator();
            ui.label(&self.log);
        });

        ctx.request_repaint();
    }
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = env::args().collect();

    let native_options = eframe::NativeOptions::default();

    if args.len() > 1 {
        // 正常进入转码器
        let file = args[1].clone();
        let app = FFUIApp {
            file,
            format: "mp4".to_string(), // 默认输出mp4
            gpu: "CPU".to_string(), // 默认用CPU处理
            progress: Arc::new(Mutex::new(0.0)),
            running: Arc::new(Mutex::new(false)),
            log_text: Arc::new(Mutex::new(String::new())),
            completed: Arc::new(Mutex::new(false)),
            child_process: Arc::new(Mutex::new(None)),
            stop_flag: Arc::new(AtomicBool::new(false)),
        };

        eframe::run_native(
            "FFUI",
            native_options,
            Box::new(|cc| {
                setup_fonts(&cc.egui_ctx);
                Box::new(app)
            }),
        )
    } else {
        // 无参数时打开右键菜单管理界面
        let app = ContextMenuApp {
            log: "将本程序添加到Windows右键菜单".to_string(),
        };
        eframe::run_native(
            "FFUI 右键菜单设置",
            native_options,
            Box::new(|cc| {
                setup_fonts(&cc.egui_ctx);
                Box::new(app)
            }),
        )
    }
}
