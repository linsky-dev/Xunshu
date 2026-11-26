//! Linux 桌面快捷方式和会话总线托盘。
use anyhow::{Context, Result};
use eframe::egui;
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, OnceLock,
    },
    time::Duration,
};
use zbus::{
    blocking::{connection::Builder, fdo::DBusProxy, Proxy},
    zvariant::{OwnedObjectPath, OwnedValue, Str, Value},
};

const ITEM: &str = "/StatusNotifierItem";
const MENU: &str = "/MenuBar";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const SHORTCUT: &[u8] = include_bytes!("../../../assets/xunshu-test.desktop");

pub fn desktop_shortcut(remove: bool) -> Result<()> {
    // 安装脚本以图形会话用户运行此入口，禁止由 root 直接写用户目录。
    if unsafe { libc::geteuid() } == 0 {
        return Ok(());
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    let output = Command::new("/usr/bin/xdg-user-dir")
        .arg("DESKTOP")
        .output()?;
    if !output.status.success() {
        return Ok(());
    }
    let desktop = Path::new(std::str::from_utf8(&output.stdout)?.trim_end_matches(['\n', '\r']));
    if desktop == home || !desktop.is_dir() || !xs_platform::local_path_within(desktop, &home)? {
        return Ok(());
    }
    xs_platform::validate_local_path(desktop, false)?;
    write_shortcut(&desktop.join("寻枢测试候选版.desktop"), remove)
}

fn write_shortcut(path: &Path, remove: bool) -> Result<()> {
    if !remove && !path.exists() {
        xs_platform::atomic_write_local(path, SHORTCUT, false)?;
    }
    if !path.exists() {
        return Ok(());
    }
    let file = xs_platform::open_local_read(path)?;
    let mut bytes = Vec::new();
    (&file)
        .take(SHORTCUT.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes == SHORTCUT {
        if remove {
            xs_platform::remove_local_file(path)?;
        } else {
            let mut permissions = file.metadata()?.permissions();
            permissions.set_mode(permissions.mode() | 0o100);
            file.set_permissions(permissions)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub enum Action {
    Open,
    Quit,
}

#[derive(Clone)]
struct Commands {
    sender: Sender<Action>,
    context: Arc<OnceLock<egui::Context>>,
}

impl Commands {
    fn send(&self, action: Action) {
        let _ = self.sender.send(action);
        if let Some(ctx) = self.context.get() {
            ctx.request_repaint();
        }
    }
}

struct Item {
    commands: Commands,
    pixels: Vec<(i32, i32, Vec<u8>)>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    fn activate(&self, _x: i32, _y: i32) {
        self.commands.send(Action::Open);
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {
        self.commands.send(Action::Open);
    }

    fn context_menu(&self, _x: i32, _y: i32) {
        self.commands.send(Action::Open);
    }

    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "xunshu-test"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "寻枢"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "xunshu-test"
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        self.pixels.clone()
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        MENU.try_into().expect("有效的固定菜单路径")
    }
}

type Properties = HashMap<String, OwnedValue>;
type Layout = (i32, Properties, Vec<OwnedValue>);

struct Menu(Commands);

fn menu_item(id: i32) -> Layout {
    let mut properties = Properties::new();
    if id == 0 {
        properties.insert("children-display".into(), Str::from("submenu").into());
    } else if matches!(id, 1 | 2) {
        let label = if id == 1 {
            "打开寻枢"
        } else {
            "退出寻枢"
        };
        properties.insert("label".into(), Str::from(label).into());
        properties.insert("enabled".into(), true.into());
        properties.insert("visible".into(), true.into());
    }
    (id, properties, Vec::new())
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        let mut root = menu_item(parent_id);
        if parent_id == 0 && recursion_depth != 0 {
            for id in [1, 2] {
                root.2.push(
                    OwnedValue::try_from(Value::new(menu_item(id)))
                        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
                );
            }
        }
        Ok((1, root))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, Properties)> {
        ids.into_iter()
            .filter(|id| (0..=2).contains(id))
            .map(|id| (id, menu_item(id).1))
            .collect()
    }

    fn event(&self, id: i32, event_id: &str, _data: Value<'_>, _timestamp: u32) {
        if event_id == "clicked" {
            match id {
                1 => self.0.send(Action::Open),
                2 => self.0.send(Action::Quit),
                _ => {}
            }
        }
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        4
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }
}

pub struct Tray {
    pub events: Receiver<Action>,
    pub ready: Arc<AtomicBool>,
    context: Arc<OnceLock<egui::Context>>,
    stop: Arc<AtomicBool>,
}

impl Tray {
    /// 返回空值表示已将启动请求转发给同配置的现有实例。
    pub fn start(profile: &Path) -> Result<Option<Self>> {
        let icon = image::load_from_memory(include_bytes!("../../../assets/xunshu-tray.png"))?
            .into_rgba8();
        let pixels = icon
            .pixels()
            .flat_map(|p| [p[3], p[0], p[1], p[2]])
            .collect();
        let (sender, events) = mpsc::channel();
        let context = Arc::new(OnceLock::new());
        let commands = Commands {
            sender,
            context: context.clone(),
        };
        let connection = Builder::session()?
            .method_timeout(Duration::from_millis(1200))
            .serve_at(
                ITEM,
                Item {
                    commands: commands.clone(),
                    pixels: vec![(icon.width() as i32, icon.height() as i32, pixels)],
                },
            )?
            .serve_at(MENU, Menu(commands))?
            .build()?;
        let mut hash = DefaultHasher::new();
        profile
            .components()
            .for_each(|component| component.hash(&mut hash));
        let name = format!("org.xunshu.Desktop.p{:016x}", hash.finish());
        match connection.request_name(name.as_str()) {
            Ok(()) => {}
            Err(zbus::Error::NameTaken) => {
                let existing = Proxy::new(
                    &connection,
                    name.as_str(),
                    ITEM,
                    "org.kde.StatusNotifierItem",
                )?;
                existing.call::<_, _, ()>("Activate", &(0_i32, 0_i32))?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
        let ready = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let monitor_ready = ready.clone();
        let monitor_stop = stop.clone();
        let monitor_context = context.clone();
        std::thread::Builder::new()
            .name("xunshu-linux-tray".into())
            .spawn(move || {
                let mut registered = None;
                while !monitor_stop.load(Ordering::Relaxed) {
                    let probe = || -> Result<Option<String>> {
                        let bus = DBusProxy::new(&connection)?;
                        let owner = bus.get_name_owner(WATCHER.try_into()?)?.to_string();
                        let watcher =
                            Proxy::new(&connection, WATCHER, "/StatusNotifierWatcher", WATCHER)?;
                        if !watcher.get_property::<bool>("IsStatusNotifierHostRegistered")? {
                            return Ok(None);
                        }
                        if registered.as_ref() != Some(&owner) {
                            watcher
                                .call::<_, _, ()>("RegisterStatusNotifierItem", &(name.as_str(),))
                                .context("托盘注册失败")?;
                        }
                        Ok(Some(owner))
                    };
                    registered = probe().ok().flatten();
                    let live = registered.is_some();
                    if monitor_ready.swap(live, Ordering::Relaxed) != live {
                        if let Some(ctx) = monitor_context.get() {
                            ctx.request_repaint();
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1500));
                }
            })?;
        Ok(Some(Self {
            events,
            ready,
            context,
            stop,
        }))
    }

    pub fn attach(&self, ctx: &egui::Context) {
        let _ = self.context.set(ctx.clone());
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_creation_and_cleanup_preserve_user_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("xunshu.desktop");
        write_shortcut(&path, false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), SHORTCUT);
        assert_ne!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o100,
            0
        );
        write_shortcut(&path, true).unwrap();
        assert!(!path.exists());
        std::fs::write(&path, b"user shortcut").unwrap();
        write_shortcut(&path, false).unwrap();
        write_shortcut(&path, true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"user shortcut");
    }
}
