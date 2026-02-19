use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::CompositorError;

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub main_key: MainKey,
    pub keybinds: Vec<Keybind>,
    pub autostart: Vec<String>,
    pub terminal: String,
    pub launcher: String,
    pub focus_follow_mouse: bool,
    pub no_csd: bool,
    pub border_size: u32,
    pub gaps_outer_horizontal: u32,
    pub gaps_outer_vertical: u32,
    pub gaps_inner_horizontal: u32,
    pub gaps_inner_vertical: u32,
    pub master_factor: f32,
    pub num_master: i32,
    pub smart_gaps: bool,
    pub cursor_theme: String,
    pub cursor_size: u32,
    pub monitors: Vec<MonitorConfig>,
    pub window_rules: Vec<WindowRule>,
    pub wallpaper: WallpaperConfig,
    pub xwayland: XwaylandConfig,
}

#[derive(Clone, Debug, Default)]
pub struct WindowRule {
    pub class: Option<String>,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub workspace: Option<usize>,
    pub floating: Option<bool>,
    pub fullscreen: Option<bool>,
    pub focus: Option<bool>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl WindowRule {
    pub fn matches(&self, app_id: Option<&str>, title: Option<&str>) -> bool {
        if let Some(expected) = &self.class
            && !matches_ci_exact(app_id, expected)
        {
            return false;
        }
        if let Some(expected) = &self.app_id
            && !matches_ci_exact(app_id, expected)
        {
            return false;
        }
        if let Some(expected) = &self.title
            && !matches_ci_contains(title, expected)
        {
            return false;
        }
        true
    }
}

#[derive(Clone, Debug)]
pub struct WallpaperConfig {
    pub enabled: bool,
    pub restore_command: String,
    pub image: String,
    pub resize: String,
    pub transition_type: String,
    pub transition_duration: f32,
}

#[derive(Clone, Debug)]
pub struct XwaylandConfig {
    pub enabled: bool,
    pub path: String,
    pub display: String,
}

#[derive(Clone, Debug)]
pub struct MonitorConfig {
    pub name: String,
    pub enabled: bool,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub refresh_hz: Option<f64>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub scale: Option<f64>,
    pub transform: Option<String>,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            width: None,
            height: None,
            refresh_hz: None,
            x: None,
            y: None,
            scale: None,
            transform: None,
        }
    }
}

impl Default for WallpaperConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            restore_command: "waypaper --restore".to_owned(),
            image: String::new(),
            resize: "crop".to_owned(),
            transition_type: "simple".to_owned(),
            transition_duration: 0.7,
        }
    }
}

impl Default for XwaylandConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "xwayland-satellite".to_owned(),
            // Empty means "auto-pick a free DISPLAY" at runtime.
            display: String::new(),
        }
    }
}

impl RuntimeConfig {
    pub fn keybind_action_for(
        &self,
        modifiers: &ModifiersState,
        keysym: Keysym,
    ) -> Option<KeybindAction> {
        self.keybinds
            .iter()
            .find(|bind| bind.matches(modifiers, keysym))
            .map(|bind| bind.action.clone())
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let main_key = MainKey::Super;
        let keybinds =
            default_keybinds(main_key).expect("default keybinds are static and must be valid");
        Self {
            main_key,
            keybinds,
            autostart: Vec::new(),
            terminal: "weston-terminal".to_owned(),
            launcher: "rofi -show drun".to_owned(),
            focus_follow_mouse: true,
            no_csd: true,
            border_size: 2,
            gaps_outer_horizontal: 20,
            gaps_outer_vertical: 20,
            gaps_inner_horizontal: 10,
            gaps_inner_vertical: 10,
            master_factor: 0.55,
            num_master: 1,
            smart_gaps: true,
            cursor_theme: "default".to_owned(),
            cursor_size: 24,
            monitors: Vec::new(),
            window_rules: Vec::new(),
            wallpaper: WallpaperConfig::default(),
            xwayland: XwaylandConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainKey {
    Super,
    Alt,
    Ctrl,
}

impl MainKey {
    pub fn matches(self, modifiers: &ModifiersState) -> bool {
        match self {
            MainKey::Super => modifiers.logo,
            MainKey::Alt => modifiers.alt,
            MainKey::Ctrl => modifiers.ctrl,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Keybind {
    pub modifiers: KeybindModifiers,
    pub key: String,
    pub action: KeybindAction,
}

impl Keybind {
    fn matches(&self, modifiers: &ModifiersState, keysym: Keysym) -> bool {
        self.modifiers.matches(modifiers) && keysym_matches_token(keysym, &self.key)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct KeybindModifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
}

impl KeybindModifiers {
    fn set_main_key(&mut self, main_key: MainKey) {
        match main_key {
            MainKey::Super => self.logo = true,
            MainKey::Alt => self.alt = true,
            MainKey::Ctrl => self.ctrl = true,
        }
    }

    fn matches(self, modifiers: &ModifiersState) -> bool {
        self.shift == modifiers.shift
            && self.ctrl == modifiers.ctrl
            && self.alt == modifiers.alt
            && self.logo == modifiers.logo
    }
}

#[derive(Clone, Debug)]
pub enum KeybindAction {
    Exec(String),
    Terminal,
    Launcher,
    CloseFocused,
    ToggleFullscreen,
    ToggleFloating,
    Quit,
    FocusNext,
    FocusPrevious,
    ReloadConfig,
    SwitchWorkspace(usize),
    MoveFocusedToWorkspace(usize),
    Unsupported(String),
}

pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: RuntimeConfig,
}

pub fn load_or_create_default() -> Result<LoadedConfig, CompositorError> {
    let path = config_path()?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                CompositorError::Backend(format!(
                    "failed to create config directory {}: {err}",
                    parent.display()
                ))
            })?;
        }
        fs::write(&path, default_config_template()).map_err(|err| {
            CompositorError::Backend(format!(
                "failed to write default config {}: {err}",
                path.display()
            ))
        })?;
        tracing::info!(path = %path.display(), "created default config.lua");
    }

    let config = load_from_path(&path)?;
    Ok(LoadedConfig { path, config })
}

pub fn load_from_path(path: &Path) -> Result<RuntimeConfig, CompositorError> {
    if !path.exists() {
        return Err(CompositorError::Backend(format!(
            "config file not found: {}",
            path.display()
        )));
    }

    let content = fs::read_to_string(path).map_err(|err| {
        CompositorError::Backend(format!("failed to read config {}: {err}", path.display()))
    })?;
    if content.trim().is_empty() {
        fs::write(path, default_config_template()).map_err(|err| {
            CompositorError::Backend(format!(
                "failed to write default config {}: {err}",
                path.display()
            ))
        })?;
        tracing::info!(path = %path.display(), "config.lua was empty; wrote default config");
    }

    let values = load_lua_values(path)?;

    let mut config = RuntimeConfig::default();

    if let Some(value) = values.get("main_key") {
        config.main_key = parse_main_key(value)?;
    }
    if let Some(value) = values.get("modkey") {
        config.main_key = parse_main_key(value)?;
    }

    if let Some(value) = values.get("terminal") {
        config.terminal = value.clone();
    }
    if let Some(value) = values.get("launcher") {
        config.launcher = value.clone();
    }
    config.focus_follow_mouse =
        parse_bool_flexible(&values, "focus_follow_mouse", config.focus_follow_mouse)?;
    config.no_csd = parse_bool_flexible(&values, "no_csd", config.no_csd)?;
    config.border_size = parse_u32(&values, "border_size", config.border_size)?;

    if let Some(gap_size) = parse_optional_u32(&values, "gap_size")? {
        config.gaps_outer_horizontal = gap_size;
        config.gaps_outer_vertical = gap_size;
        config.gaps_inner_horizontal = gap_size;
        config.gaps_inner_vertical = gap_size;
    }

    config.gaps_outer_horizontal = parse_u32(
        &values,
        "gaps.outer_horizontal",
        config.gaps_outer_horizontal,
    )?;
    config.gaps_outer_vertical =
        parse_u32(&values, "gaps.outer_vertical", config.gaps_outer_vertical)?;
    config.gaps_inner_horizontal = parse_u32(
        &values,
        "gaps.inner_horizontal",
        config.gaps_inner_horizontal,
    )?;
    config.gaps_inner_vertical =
        parse_u32(&values, "gaps.inner_vertical", config.gaps_inner_vertical)?;

    config.master_factor = parse_f32(&values, "master_factor", config.master_factor)?;
    if !(0.1..=0.9).contains(&config.master_factor) {
        return Err(CompositorError::Backend(
            "master_factor must be between 0.1 and 0.9".to_owned(),
        ));
    }

    config.num_master = parse_i32(&values, "num_master", config.num_master)?;
    if config.num_master < 1 {
        return Err(CompositorError::Backend(
            "num_master must be >= 1".to_owned(),
        ));
    }

    config.smart_gaps = parse_bool(&values, "smart_gaps", config.smart_gaps)?;

    if let Some(value) = values.get("cursor_theme") {
        config.cursor_theme = value.clone();
    }
    config.cursor_size = parse_u32(&values, "cursor_size", config.cursor_size)?;
    if config.cursor_size == 0 {
        return Err(CompositorError::Backend(
            "cursor_size must be greater than 0".to_owned(),
        ));
    }

    config.autostart = collect_indexed_values(&values, "autostart.")?;

    config.wallpaper.enabled =
        parse_bool_flexible(&values, "wallpaper.enabled", config.wallpaper.enabled)?;
    if let Some(value) = values
        .get("wallpaper.restore_command")
        .or_else(|| values.get("wallpaper.command"))
        .or_else(|| values.get("wallpaper.restore_cmd"))
        .or_else(|| values.get("wallpaper_cmd"))
    {
        config.wallpaper.restore_command = value.clone();
    }
    if let Some(value) = values.get("wallpaper.image") {
        config.wallpaper.image = value.clone();
    }
    if let Some(value) = values.get("wallpaper.resize") {
        config.wallpaper.resize = value.clone();
    }
    if let Some(value) = values.get("wallpaper.transition_type") {
        config.wallpaper.transition_type = value.clone();
    }
    config.wallpaper.transition_duration = parse_f32(
        &values,
        "wallpaper.transition_duration",
        config.wallpaper.transition_duration,
    )?;
    if config.wallpaper.transition_duration < 0.0 {
        return Err(CompositorError::Backend(
            "wallpaper.transition_duration must be >= 0".to_owned(),
        ));
    }
    if config.wallpaper.enabled
        && config.wallpaper.restore_command.trim().is_empty()
        && config.wallpaper.image.trim().is_empty()
    {
        return Err(CompositorError::Backend(
            "wallpaper.enabled is true but wallpaper.restore_command and wallpaper.image are empty"
                .to_owned(),
        ));
    }

    config.xwayland.enabled =
        parse_bool_flexible(&values, "xwayland.enabled", config.xwayland.enabled)?;
    if let Some(value) = values
        .get("xwayland.path")
        .or_else(|| values.get("xwayland_path"))
    {
        config.xwayland.path = value.clone();
    }
    if let Some(value) = values
        .get("xwayland.display")
        .or_else(|| values.get("xwayland_display"))
    {
        config.xwayland.display = value.clone();
    }
    if config.xwayland.enabled {
        if config.xwayland.path.trim().is_empty() {
            return Err(CompositorError::Backend(
                "xwayland.enabled is true but xwayland.path is empty".to_owned(),
            ));
        }
    }

    config.monitors = parse_monitor_configs(&values)?;
    config.window_rules = parse_window_rules(&values)?;

    let keybind_lines = collect_indexed_values(&values, "keybind.")?;
    config.keybinds = if keybind_lines.is_empty() {
        default_keybinds(config.main_key)?
    } else {
        keybind_lines
            .iter()
            .map(|line| parse_keybind_line(line, config.main_key))
            .collect::<Result<Vec<_>, _>>()?
    };

    Ok(config)
}

pub fn apply_environment(config: &RuntimeConfig) {
    // SAFETY: This compositor mutates process environment from the main event loop thread only.
    unsafe {
        std::env::set_var("XCURSOR_THEME", &config.cursor_theme);
        std::env::set_var("XCURSOR_SIZE", config.cursor_size.to_string());
        std::env::set_var("XDG_SESSION_TYPE", "wayland");
        std::env::set_var("XDG_CURRENT_DESKTOP", "raven");
        std::env::set_var("XDG_SESSION_DESKTOP", "raven");
        std::env::set_var("QT_QPA_PLATFORM", "wayland");
        std::env::set_var("SDL_VIDEODRIVER", "wayland");
        std::env::set_var("MOZ_ENABLE_WAYLAND", "1");
        std::env::set_var("MOZ_DBUS_REMOTE", "0");
        if config.no_csd {
            std::env::set_var("QT_WAYLAND_DISABLE_WINDOWDECORATION", "1");
        } else {
            std::env::remove_var("QT_WAYLAND_DISABLE_WINDOWDECORATION");
        }
    }
}

fn config_path() -> Result<PathBuf, CompositorError> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("raven").join("config.lua"));
    }

    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home)
            .join(".config")
            .join("raven")
            .join("config.lua"));
    }

    Err(CompositorError::Backend(
        "unable to resolve config path: HOME and XDG_CONFIG_HOME are unset".to_owned(),
    ))
}

fn load_lua_values(path: &Path) -> Result<HashMap<String, String>, CompositorError> {
    let output = Command::new("lua")
        .arg("-e")
        .arg(lua_loader_script())
        .env("RAVEN_CONFIG_PATH", path)
        .output()
        .map_err(|err| CompositorError::Backend(format!("failed to execute lua: {err}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let reason = if stderr.is_empty() {
            "lua exited with non-zero status".to_owned()
        } else {
            stderr
        };
        return Err(CompositorError::Backend(format!(
            "failed to load {}: {reason}",
            path.display()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_key_value_stdout(&stdout)
}

fn parse_key_value_stdout(stdout: &str) -> Result<HashMap<String, String>, CompositorError> {
    let mut values = HashMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(CompositorError::Backend(format!(
                "invalid lua output line: {line}"
            )));
        };
        values.insert(key.to_owned(), value.to_owned());
    }
    Ok(values)
}

fn collect_indexed_values(
    values: &HashMap<String, String>,
    prefix: &str,
) -> Result<Vec<String>, CompositorError> {
    let mut indexed = Vec::<(usize, String)>::new();

    for (key, value) in values {
        let Some(index_str) = key.strip_prefix(prefix) else {
            continue;
        };

        let index = index_str.parse::<usize>().map_err(|err| {
            CompositorError::Backend(format!(
                "invalid indexed key `{key}`: index is not a number ({err})"
            ))
        })?;
        indexed.push((index, value.clone()));
    }

    indexed.sort_by_key(|(index, _)| *index);
    Ok(indexed.into_iter().map(|(_, value)| value).collect())
}

fn parse_monitor_configs(
    values: &HashMap<String, String>,
) -> Result<Vec<MonitorConfig>, CompositorError> {
    let mut grouped = BTreeMap::<usize, HashMap<String, String>>::new();

    for (key, value) in values {
        let Some(rest) = key.strip_prefix("monitor.") else {
            continue;
        };
        let Some((raw_index, field)) = rest.split_once('.') else {
            return Err(CompositorError::Backend(format!(
                "invalid monitor key `{key}`: expected format monitor.<index>.<field>"
            )));
        };
        if field.trim().is_empty() {
            return Err(CompositorError::Backend(format!(
                "invalid monitor key `{key}`: missing field"
            )));
        }
        let index = raw_index.parse::<usize>().map_err(|err| {
            CompositorError::Backend(format!(
                "invalid monitor key `{key}`: index is not a number ({err})"
            ))
        })?;
        grouped
            .entry(index)
            .or_default()
            .insert(field.trim().to_owned(), value.clone());
    }

    let mut monitors = Vec::with_capacity(grouped.len());

    for (index, fields) in grouped {
        let monitor_name_key = format!("monitor.{index}.name");
        let name = fields
            .get("name")
            .or_else(|| fields.get("output"))
            .map(|raw| raw.trim())
            .filter(|raw| !raw.is_empty())
            .ok_or_else(|| {
                CompositorError::Backend(format!(
                    "missing `{monitor_name_key}` (or `monitor.{index}.output`) in monitor config"
                ))
            })?;

        let mut monitor = MonitorConfig {
            name: name.to_owned(),
            ..MonitorConfig::default()
        };

        let has_mode = fields
            .get("mode")
            .map(|raw| !raw.trim().is_empty())
            .unwrap_or(false);
        let has_explicit_size = fields.contains_key("width") || fields.contains_key("height");
        let has_explicit_refresh = fields.contains_key("refresh_hz")
            || fields.contains_key("refresh")
            || fields.contains_key("hz");

        if has_mode && (has_explicit_size || has_explicit_refresh) {
            return Err(CompositorError::Backend(format!(
                "monitor `{}`: `mode` cannot be combined with width/height/refresh fields",
                monitor.name
            )));
        }

        if let Some(enabled) = parse_optional_bool_flexible_in_map(
            &fields,
            "enabled",
            &format!(
                "monitor.{monitor_name}.enabled",
                monitor_name = monitor.name
            ),
        )? {
            monitor.enabled = enabled;
        }

        if has_mode {
            let mode_raw = fields.get("mode").expect("has_mode checked");
            let (width, height, refresh_hz) = parse_monitor_mode(
                mode_raw,
                &format!("monitor.{monitor_name}.mode", monitor_name = monitor.name),
            )?;
            monitor.width = Some(width);
            monitor.height = Some(height);
            monitor.refresh_hz = refresh_hz;
        } else {
            monitor.width = parse_optional_u16_flexible_in_map(
                &fields,
                "width",
                &format!("monitor.{monitor_name}.width", monitor_name = monitor.name),
            )?;
            monitor.height = parse_optional_u16_flexible_in_map(
                &fields,
                "height",
                &format!("monitor.{monitor_name}.height", monitor_name = monitor.name),
            )?;

            if monitor.width.is_some() ^ monitor.height.is_some() {
                return Err(CompositorError::Backend(format!(
                    "monitor `{}`: width and height must be set together",
                    monitor.name
                )));
            }

            monitor.refresh_hz = parse_optional_f64_in_map(
                &fields,
                "refresh_hz",
                &format!(
                    "monitor.{monitor_name}.refresh_hz",
                    monitor_name = monitor.name
                ),
            )?;
            if monitor.refresh_hz.is_none() {
                monitor.refresh_hz = parse_optional_f64_in_map(
                    &fields,
                    "refresh",
                    &format!(
                        "monitor.{monitor_name}.refresh",
                        monitor_name = monitor.name
                    ),
                )?;
            }
            if monitor.refresh_hz.is_none() {
                monitor.refresh_hz = parse_optional_f64_in_map(
                    &fields,
                    "hz",
                    &format!("monitor.{monitor_name}.hz", monitor_name = monitor.name),
                )?;
            }

            if let Some(refresh_hz) = monitor.refresh_hz
                && refresh_hz <= 0.0
            {
                return Err(CompositorError::Backend(format!(
                    "monitor `{}`: refresh_hz must be greater than 0",
                    monitor.name
                )));
            }
        }

        monitor.x = parse_optional_i32_flexible_in_map(
            &fields,
            "x",
            &format!("monitor.{monitor_name}.x", monitor_name = monitor.name),
        )?;
        monitor.y = parse_optional_i32_flexible_in_map(
            &fields,
            "y",
            &format!("monitor.{monitor_name}.y", monitor_name = monitor.name),
        )?;
        monitor.scale = parse_optional_f64_in_map(
            &fields,
            "scale",
            &format!("monitor.{monitor_name}.scale", monitor_name = monitor.name),
        )?;

        if let Some(scale) = monitor.scale
            && scale <= 0.0
        {
            return Err(CompositorError::Backend(format!(
                "monitor `{}`: scale must be greater than 0",
                monitor.name
            )));
        }

        monitor.transform = fields
            .get("transform")
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());

        monitors.push(monitor);
    }

    Ok(monitors)
}

fn parse_window_rules(values: &HashMap<String, String>) -> Result<Vec<WindowRule>, CompositorError> {
    let mut grouped = BTreeMap::<usize, HashMap<String, String>>::new();

    for (key, value) in values {
        let Some(rest) = key.strip_prefix("window_rule.") else {
            continue;
        };
        let Some((raw_index, field)) = rest.split_once('.') else {
            return Err(CompositorError::Backend(format!(
                "invalid window rule key `{key}`: expected format window_rule.<index>.<field>"
            )));
        };
        if field.trim().is_empty() {
            return Err(CompositorError::Backend(format!(
                "invalid window rule key `{key}`: missing field"
            )));
        }
        let index = raw_index.parse::<usize>().map_err(|err| {
            CompositorError::Backend(format!(
                "invalid window rule key `{key}`: index is not a number ({err})"
            ))
        })?;
        grouped
            .entry(index)
            .or_default()
            .insert(field.trim().to_owned(), value.clone());
    }

    let mut rules = Vec::with_capacity(grouped.len());
    for (index, fields) in grouped {
        let mut rule = WindowRule::default();
        rule.class = normalize_non_empty_field(&fields, "class");
        rule.app_id = normalize_non_empty_field(&fields, "app_id")
            .or_else(|| normalize_non_empty_field(&fields, "appid"));
        rule.title = normalize_non_empty_field(&fields, "title");
        rule.workspace = parse_window_rule_workspace(&fields, index)?;
        rule.floating = parse_optional_bool_flexible_in_map(
            &fields,
            "floating",
            &format!("window_rule.{index}.floating"),
        )?;
        rule.fullscreen = parse_optional_bool_flexible_in_map(
            &fields,
            "fullscreen",
            &format!("window_rule.{index}.fullscreen"),
        )?;
        rule.focus = parse_optional_bool_flexible_in_map(
            &fields,
            "focus",
            &format!("window_rule.{index}.focus"),
        )?;
        rule.width = parse_optional_u32_in_map(
            &fields,
            "width",
            &format!("window_rule.{index}.width"),
        )?;
        rule.height = parse_optional_u32_in_map(
            &fields,
            "height",
            &format!("window_rule.{index}.height"),
        )?;

        rules.push(rule);
    }

    Ok(rules)
}

fn normalize_non_empty_field(fields: &HashMap<String, String>, field: &str) -> Option<String> {
    fields
        .get(field)
        .map(|raw| raw.trim())
        .filter(|raw| !raw.is_empty())
        .map(|raw| raw.to_owned())
}

fn parse_window_rule_workspace(
    fields: &HashMap<String, String>,
    index: usize,
) -> Result<Option<usize>, CompositorError> {
    let Some(raw) = fields.get("workspace").map(String::as_str) else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let number = trimmed.parse::<usize>().map_err(|err| {
        CompositorError::Backend(format!(
            "invalid value for window_rule.{index}.workspace: {trimmed} ({err})"
        ))
    })?;

    if !(1..=10).contains(&number) {
        return Err(CompositorError::Backend(format!(
            "invalid value for window_rule.{index}.workspace: {trimmed} (expected 1..10)"
        )));
    }

    Ok(Some(number - 1))
}

fn parse_optional_bool_flexible_in_map(
    fields: &HashMap<String, String>,
    field: &str,
    key: &str,
) -> Result<Option<bool>, CompositorError> {
    let Some(raw) = fields.get(field) else {
        return Ok(None);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(Some(true)),
        "false" | "0" | "no" | "off" => Ok(Some(false)),
        _ => Err(CompositorError::Backend(format!(
            "invalid value for {key}: {raw} (expected bool or 0/1)"
        ))),
    }
}

fn parse_optional_u32_in_map(
    fields: &HashMap<String, String>,
    field: &str,
    key: &str,
) -> Result<Option<u32>, CompositorError> {
    let Some(raw) = fields.get(field) else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed.parse::<u32>().map(Some).map_err(|err| {
        CompositorError::Backend(format!("invalid value for {key}: {trimmed} ({err})"))
    })
}

fn parse_optional_u16_flexible_in_map(
    fields: &HashMap<String, String>,
    field: &str,
    key: &str,
) -> Result<Option<u16>, CompositorError> {
    let Some(raw) = fields.get(field) else {
        return Ok(None);
    };
    parse_u16_flexible(raw, key).map(Some)
}

fn parse_u16_flexible(raw: &str, key: &str) -> Result<u16, CompositorError> {
    if let Ok(number) = raw.parse::<u16>() {
        return Ok(number);
    }

    let parsed = raw.parse::<f64>().map_err(|err| {
        CompositorError::Backend(format!("invalid value for {key}: {raw} ({err})"))
    })?;
    if !parsed.is_finite() || parsed < 0.0 || parsed > u16::MAX as f64 || parsed.fract() != 0.0 {
        return Err(CompositorError::Backend(format!(
            "invalid value for {key}: {raw} (expected non-negative integer <= {})",
            u16::MAX
        )));
    }

    Ok(parsed as u16)
}

fn parse_optional_i32_flexible_in_map(
    fields: &HashMap<String, String>,
    field: &str,
    key: &str,
) -> Result<Option<i32>, CompositorError> {
    let Some(raw) = fields.get(field) else {
        return Ok(None);
    };
    parse_i32_flexible(raw, key).map(Some)
}

fn parse_i32_flexible(raw: &str, key: &str) -> Result<i32, CompositorError> {
    if let Ok(number) = raw.parse::<i32>() {
        return Ok(number);
    }

    let parsed = raw.parse::<f64>().map_err(|err| {
        CompositorError::Backend(format!("invalid value for {key}: {raw} ({err})"))
    })?;
    if !parsed.is_finite()
        || parsed < i32::MIN as f64
        || parsed > i32::MAX as f64
        || parsed.fract() != 0.0
    {
        return Err(CompositorError::Backend(format!(
            "invalid value for {key}: {raw} (expected integer)"
        )));
    }

    Ok(parsed as i32)
}

fn parse_optional_f64_in_map(
    fields: &HashMap<String, String>,
    field: &str,
    key: &str,
) -> Result<Option<f64>, CompositorError> {
    let Some(raw) = fields.get(field) else {
        return Ok(None);
    };
    let value = raw.parse::<f64>().map_err(|err| {
        CompositorError::Backend(format!("invalid value for {key}: {raw} ({err})"))
    })?;
    if !value.is_finite() {
        return Err(CompositorError::Backend(format!(
            "invalid value for {key}: {raw} (must be finite)"
        )));
    }
    Ok(Some(value))
}

fn parse_monitor_mode(raw: &str, key: &str) -> Result<(u16, u16, Option<f64>), CompositorError> {
    let mode = raw.trim();
    if mode.is_empty() {
        return Err(CompositorError::Backend(format!(
            "invalid value for {key}: mode must not be empty"
        )));
    }

    let (size_part, refresh_part) = match mode.split_once('@') {
        Some((size, refresh)) => (size.trim(), Some(refresh.trim())),
        None => (mode, None),
    };

    let split_size = size_part
        .split_once('x')
        .or_else(|| size_part.split_once('X'))
        .ok_or_else(|| {
            CompositorError::Backend(format!(
                "invalid value for {key}: expected `<width>x<height>` or `<width>x<height>@<refresh>`"
            ))
        })?;

    let width = parse_u16_flexible(split_size.0.trim(), key)?;
    let height = parse_u16_flexible(split_size.1.trim(), key)?;

    let refresh_hz = match refresh_part {
        Some(raw_refresh) => {
            if raw_refresh.is_empty() {
                return Err(CompositorError::Backend(format!(
                    "invalid value for {key}: refresh rate must not be empty"
                )));
            }
            let parsed = raw_refresh.parse::<f64>().map_err(|err| {
                CompositorError::Backend(format!(
                    "invalid value for {key}: invalid refresh `{raw_refresh}` ({err})"
                ))
            })?;
            if !parsed.is_finite() || parsed <= 0.0 {
                return Err(CompositorError::Backend(format!(
                    "invalid value for {key}: refresh must be greater than 0"
                )));
            }
            Some(parsed)
        }
        None => None,
    };

    Ok((width, height, refresh_hz))
}

fn parse_main_key(raw: &str) -> Result<MainKey, CompositorError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "super" | "logo" | "win" | "windows" | "mod4" => Ok(MainKey::Super),
        "alt" | "mod1" => Ok(MainKey::Alt),
        "ctrl" | "control" => Ok(MainKey::Ctrl),
        _ => Err(CompositorError::Backend(format!(
            "invalid main_key/modkey `{raw}` (expected Super/Mod4, Alt/Mod1, or Ctrl)"
        ))),
    }
}

fn default_keybinds(main_key: MainKey) -> Result<Vec<Keybind>, CompositorError> {
    const DEFAULT_BINDS: &[&str] = &[
        "Main+Return terminal",
        "Main+D launcher",
        "Main+Q close",
        "Main+V toggle_floating",
        "Main+J focus_next",
        "Main+K focus_previous",
        "Main+Shift+R reload_config",
        "Main+Escape quit",
    ];

    DEFAULT_BINDS
        .iter()
        .map(|line| parse_keybind_line(line, main_key))
        .collect()
}

fn parse_keybind_line(line: &str, main_key: MainKey) -> Result<Keybind, CompositorError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(CompositorError::Backend(
            "keybind entry must not be empty".to_owned(),
        ));
    }

    let mut parts = trimmed.split_whitespace();
    let combo = parts.next().expect("already checked");
    let action_name = parts.next().ok_or_else(|| {
        CompositorError::Backend(format!(
            "invalid keybind `{trimmed}`: missing action (expected format `<combo> <action> [args]`)"
        ))
    })?;
    let action_args = parts.collect::<Vec<_>>().join(" ");

    let (modifiers, key) = parse_combo(combo, main_key)?;
    let action = parse_keybind_action(action_name, action_args.as_str(), trimmed)?;

    Ok(Keybind {
        modifiers,
        key,
        action,
    })
}

fn parse_combo(
    combo: &str,
    main_key: MainKey,
) -> Result<(KeybindModifiers, String), CompositorError> {
    let mut modifiers = KeybindModifiers::default();
    let mut key: Option<String> = None;

    for raw_part in combo.split('+') {
        let part = raw_part.trim();
        if part.is_empty() {
            return Err(CompositorError::Backend(format!(
                "invalid key combo `{combo}`: empty segment"
            )));
        }

        match part.to_ascii_lowercase().as_str() {
            "shift" => modifiers.shift = true,
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" | "mod1" => modifiers.alt = true,
            "super" | "logo" | "win" | "windows" | "mod4" => modifiers.logo = true,
            "main" => modifiers.set_main_key(main_key),
            _ => {
                if key.is_some() {
                    return Err(CompositorError::Backend(format!(
                        "invalid key combo `{combo}`: multiple key tokens"
                    )));
                }
                key = Some(normalize_key_token(part));
            }
        }
    }

    let key = key.ok_or_else(|| {
        CompositorError::Backend(format!(
            "invalid key combo `{combo}`: missing non-modifier key"
        ))
    })?;

    Ok((modifiers, key))
}

fn normalize_key_token(raw: &str) -> String {
    if raw.len() == 1 {
        let ch = raw.chars().next().expect("len checked");
        if ch.is_ascii_alphabetic() {
            return ch.to_ascii_uppercase().to_string();
        }
        return ch.to_string();
    }

    match raw.to_ascii_uppercase().as_str() {
        "ENTER" => "RETURN".to_owned(),
        "ESC" => "ESCAPE".to_owned(),
        "SPACEBAR" => "SPACE".to_owned(),
        other => other.to_owned(),
    }
}

fn parse_keybind_action(
    action_name: &str,
    action_args: &str,
    full_line: &str,
) -> Result<KeybindAction, CompositorError> {
    let action = match action_name.to_ascii_lowercase().as_str() {
        "exec" => {
            if action_args.trim().is_empty() {
                return Err(CompositorError::Backend(format!(
                    "invalid keybind `{full_line}`: `exec` requires a command"
                )));
            }
            KeybindAction::Exec(action_args.trim().to_owned())
        }
        "terminal" => KeybindAction::Terminal,
        "launcher" => KeybindAction::Launcher,
        "close" | "close_focused" => KeybindAction::CloseFocused,
        "close_window" => KeybindAction::CloseFocused,
        "fullscreen" | "togglefullscreen" => KeybindAction::ToggleFullscreen,
        "toggle_floating" | "togglefloating" | "floating" => KeybindAction::ToggleFloating,
        "quit" => KeybindAction::Quit,
        "focus_next" | "next" => KeybindAction::FocusNext,
        "focus_prev" | "focus_previous" | "prev" => KeybindAction::FocusPrevious,
        "reload" | "reload_config" => KeybindAction::ReloadConfig,
        "workspace" => KeybindAction::SwitchWorkspace(parse_workspace_index(
            action_args,
            full_line,
            "workspace",
        )?),
        "movetoworkspace" => KeybindAction::MoveFocusedToWorkspace(parse_workspace_index(
            action_args,
            full_line,
            "movetoworkspace",
        )?),
        "resize_left" | "resize_right" | "swap_master" => {
            KeybindAction::Unsupported(action_name.to_owned())
        }
        _ => {
            return Err(CompositorError::Backend(format!(
                "invalid keybind `{full_line}`: unknown action `{action_name}`"
            )));
        }
    };

    if !matches!(
        action,
        KeybindAction::Exec(_)
            | KeybindAction::SwitchWorkspace(_)
            | KeybindAction::MoveFocusedToWorkspace(_)
    ) && !action_args.trim().is_empty()
    {
        return Err(CompositorError::Backend(format!(
            "invalid keybind `{full_line}`: action `{action_name}` does not accept arguments"
        )));
    }

    Ok(action)
}

fn parse_workspace_index(
    action_args: &str,
    full_line: &str,
    action_name: &str,
) -> Result<usize, CompositorError> {
    let raw = action_args.trim();
    if raw.is_empty() {
        return Err(CompositorError::Backend(format!(
            "invalid keybind `{full_line}`: action `{action_name}` requires workspace number"
        )));
    }
    if raw.contains(char::is_whitespace) {
        return Err(CompositorError::Backend(format!(
            "invalid keybind `{full_line}`: action `{action_name}` expects a single number"
        )));
    }

    let number = raw.parse::<usize>().map_err(|err| {
        CompositorError::Backend(format!(
            "invalid keybind `{full_line}`: invalid workspace number `{raw}` ({err})"
        ))
    })?;

    if !(1..=10).contains(&number) {
        return Err(CompositorError::Backend(format!(
            "invalid keybind `{full_line}`: workspace must be between 1 and 10"
        )));
    }

    Ok(number - 1)
}

fn matches_ci_exact(actual: Option<&str>, expected: &str) -> bool {
    actual.is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn matches_ci_contains(actual: Option<&str>, expected: &str) -> bool {
    let expected = expected.to_ascii_lowercase();
    actual
        .map(str::to_ascii_lowercase)
        .is_some_and(|value| value.contains(&expected))
}

fn keysym_matches_token(keysym: Keysym, token: &str) -> bool {
    if token.len() == 1 {
        let token_char = token.chars().next().expect("len checked");

        if token_char.is_ascii_digit() {
            return digit_matches_keysym(token_char, keysym);
        }

        if token_char.is_ascii_alphabetic() {
            return keysym
                .key_char()
                .is_some_and(|ch| ch.eq_ignore_ascii_case(&token_char));
        }

        return keysym.key_char() == Some(token_char);
    }

    match token {
        "RETURN" => matches!(keysym, Keysym::Return | Keysym::KP_Enter),
        "ESCAPE" => keysym == Keysym::Escape,
        "PRINT" => keysym == Keysym::Print,
        "SPACE" => keysym.key_char() == Some(' '),
        "TAB" => matches!(keysym, Keysym::Tab | Keysym::ISO_Left_Tab | Keysym::KP_Tab),
        "LEFT" => keysym == Keysym::Left,
        "RIGHT" => keysym == Keysym::Right,
        "UP" => keysym == Keysym::Up,
        "DOWN" => keysym == Keysym::Down,
        "BACKSPACE" => keysym == Keysym::BackSpace,
        _ => false,
    }
}

fn digit_matches_keysym(digit: char, keysym: Keysym) -> bool {
    match digit {
        '1' => matches!(keysym, Keysym::_1 | Keysym::exclam),
        '2' => matches!(keysym, Keysym::_2 | Keysym::at),
        '3' => matches!(keysym, Keysym::_3 | Keysym::numbersign),
        '4' => matches!(keysym, Keysym::_4 | Keysym::dollar),
        '5' => matches!(keysym, Keysym::_5 | Keysym::percent),
        '6' => matches!(keysym, Keysym::_6 | Keysym::asciicircum),
        '7' => matches!(keysym, Keysym::_7 | Keysym::ampersand),
        '8' => matches!(keysym, Keysym::_8 | Keysym::asterisk),
        '9' => matches!(keysym, Keysym::_9 | Keysym::parenleft),
        '0' => matches!(keysym, Keysym::_0 | Keysym::parenright),
        _ => false,
    }
}

fn parse_u32(
    values: &HashMap<String, String>,
    key: &str,
    default: u32,
) -> Result<u32, CompositorError> {
    match values.get(key) {
        Some(raw) => raw.parse::<u32>().map_err(|err| {
            CompositorError::Backend(format!("invalid value for {key}: {raw} ({err})"))
        }),
        None => Ok(default),
    }
}

fn parse_optional_u32(
    values: &HashMap<String, String>,
    key: &str,
) -> Result<Option<u32>, CompositorError> {
    match values.get(key) {
        Some(raw) => raw.parse::<u32>().map(Some).map_err(|err| {
            CompositorError::Backend(format!("invalid value for {key}: {raw} ({err})"))
        }),
        None => Ok(None),
    }
}

fn parse_i32(
    values: &HashMap<String, String>,
    key: &str,
    default: i32,
) -> Result<i32, CompositorError> {
    match values.get(key) {
        Some(raw) => raw.parse::<i32>().map_err(|err| {
            CompositorError::Backend(format!("invalid value for {key}: {raw} ({err})"))
        }),
        None => Ok(default),
    }
}

fn parse_f32(
    values: &HashMap<String, String>,
    key: &str,
    default: f32,
) -> Result<f32, CompositorError> {
    match values.get(key) {
        Some(raw) => raw.parse::<f32>().map_err(|err| {
            CompositorError::Backend(format!("invalid value for {key}: {raw} ({err})"))
        }),
        None => Ok(default),
    }
}

fn parse_bool(
    values: &HashMap<String, String>,
    key: &str,
    default: bool,
) -> Result<bool, CompositorError> {
    match values.get(key) {
        Some(raw) => raw.parse::<bool>().map_err(|err| {
            CompositorError::Backend(format!("invalid value for {key}: {raw} ({err})"))
        }),
        None => Ok(default),
    }
}

fn parse_bool_flexible(
    values: &HashMap<String, String>,
    key: &str,
    default: bool,
) -> Result<bool, CompositorError> {
    let Some(raw) = values.get(key) else {
        return Ok(default);
    };

    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(CompositorError::Backend(format!(
            "invalid value for {key}: {raw} (expected bool or 0/1)"
        ))),
    }
}

fn default_config_template() -> &'static str {
    r#"-- Raven config
-- File: ~/.config/raven/config.lua (or $XDG_CONFIG_HOME/raven/config.lua)
return {
  general = {
    modkey = "Super",
    terminal = "foot",
    launcher = "fuzzel",
    focus_follow_mouse = true,
    no_csd = true,
    gap_size = 8,
    border_size = 0,
  },

  keybindings = {
    { combo = "Main+Q", action = "exec", command = "foot" },
    { combo = "Main+X", action = "exec", command = "firefox" },
    { combo = "Main+D", action = "exec", command = "fuzzel" },
    { combo = "Main+C", action = "close_window" },
    { combo = "Main+F", action = "fullscreen" },
    { combo = "Main+V", action = "toggle_floating" },
    { combo = "Main+J", action = "focus_next" },
    { combo = "Main+K", action = "focus_prev" },
    { combo = "Main+Shift+R", action = "reload_config" },
    { combo = "Main+Shift+Q", action = "quit" },

    { combo = "Main+1", action = "workspace", arg = "1" },
    { combo = "Main+2", action = "workspace", arg = "2" },
    { combo = "Main+3", action = "workspace", arg = "3" },
    { combo = "Main+4", action = "workspace", arg = "4" },
    { combo = "Main+5", action = "workspace", arg = "5" },
    { combo = "Main+6", action = "workspace", arg = "6" },
    { combo = "Main+7", action = "workspace", arg = "7" },
    { combo = "Main+8", action = "workspace", arg = "8" },
    { combo = "Main+9", action = "workspace", arg = "9" },
    { combo = "Main+0", action = "workspace", arg = "10" },

    { combo = "Main+Shift+1", action = "movetoworkspace", arg = "1" },
    { combo = "Main+Shift+2", action = "movetoworkspace", arg = "2" },
    { combo = "Main+Shift+3", action = "movetoworkspace", arg = "3" },
    { combo = "Main+Shift+4", action = "movetoworkspace", arg = "4" },
    { combo = "Main+Shift+5", action = "movetoworkspace", arg = "5" },
    { combo = "Main+Shift+6", action = "movetoworkspace", arg = "6" },
    { combo = "Main+Shift+7", action = "movetoworkspace", arg = "7" },
    { combo = "Main+Shift+8", action = "movetoworkspace", arg = "8" },
    { combo = "Main+Shift+9", action = "movetoworkspace", arg = "9" },
    { combo = "Main+Shift+0", action = "movetoworkspace", arg = "10" },
  },

  monitors = {
    -- Keep empty to let Raven auto-pick preferred modes for all outputs.
    -- Use your real monitor names (examples: eDP-1, DP-1, HDMI-A-1).
    -- You can list current names with: `raven monitors`.
    --
    -- Recommended keyed form:
    -- ["eDP-1"] = {
    --   -- Use ONE sizing method (do not mix):
    --   -- mode = "1920x1080@120.030"   -- or "1920x1080"
    --   -- width = 1920, height = 1080, refresh_hz = 120.030
    --
    --   scale = 1.0,                     -- integer/fractional, must be > 0
    --   transform = "normal",            -- normal/90/180/270/flipped/flipped-90/flipped-180/flipped-270
    --   position = { x = 0, y = 0 },     -- or x = 0, y = 0
    -- },
    --
    -- Disable an output:
    -- ["HDMI-A-1"] = { off = true },     -- same as enabled = false
    --
    -- Also valid (array form):
    -- { name = "DP-1", mode = "2560x1440@165", x = 1920, y = 0 },
  },

  window_rules = {
    { class = "Firefox", workspace = "2" },
    -- { class = "mpv", floating = true, width = 1280, height = 720 },
  },

  autostart = {
    "waybar",
    "mako",
  },

  wallpaper = {
    enabled = false,
    restore_command = "waypaper --restore",
    -- Note: with waypaper restore configured, Raven may still ensure swww-daemon
    -- is running at startup for compatibility, even if enabled = false.

    -- Optional legacy swww mode:
    -- Set restore_command = "" and uncomment the keys below.
    -- image = "~/Pictures/wallpaper.jpg",
    -- resize = "crop",
    -- transition_type = "simple",
    -- transition_duration = 0.7,
  },
}
"#
}

fn lua_loader_script() -> &'static str {
    r#"
local path = os.getenv("RAVEN_CONFIG_PATH")
if type(path) ~= "string" or path == "" then
  io.stderr:write("RAVEN_CONFIG_PATH is not set\n")
  os.exit(1)
end

local chunk, load_err = loadfile(path)
if not chunk then
  io.stderr:write(load_err .. "\n")
  os.exit(1)
end

local ok, result = pcall(chunk)
if not ok then
  io.stderr:write(result .. "\n")
  os.exit(1)
end

local cfg = nil
if type(result) == "table" then
  cfg = result
elseif type(_G.config) == "table" then
  cfg = _G.config
else
  cfg = {}
end

local raven_binds = {}
local general = {}

focus_next = "focus_next"
focus_prev = "focus_prev"
focus_previous = "focus_previous"
close_window = "close_window"
quit = "quit"
reload_config = "reload_config"
terminal_action = "terminal"
launcher_action = "launcher"
swap_master = "swap_master"
resize_left = "resize_left"
resize_right = "resize_right"

function spawn(command)
  return { __raven_action = "exec", command = command }
end

function bind(mods, key, action)
  if type(mods) ~= "string" then
    io.stderr:write("bind: mods must be a string\n")
    os.exit(1)
  end
  if type(key) ~= "string" then
    io.stderr:write("bind: key must be a string\n")
    os.exit(1)
  end

  local combo_parts = {}
  for part in string.gmatch(mods, "%S+") do
    if part == "Mod4" then
      table.insert(combo_parts, "Super")
    elseif part == "Mod1" then
      table.insert(combo_parts, "Alt")
    else
      table.insert(combo_parts, part)
    end
  end
  table.insert(combo_parts, key)
  local combo = table.concat(combo_parts, "+")

  local rendered_action = nil
  if type(action) == "string" then
    rendered_action = action
  elseif type(action) == "table" and action.__raven_action == "exec" then
    if type(action.command) ~= "string" then
      io.stderr:write("spawn command must be a string\n")
      os.exit(1)
    end
    rendered_action = "exec " .. action.command
  else
    io.stderr:write("bind: action must be a known action name or spawn(...)\n")
    os.exit(1)
  end

  table.insert(raven_binds, combo .. " " .. rendered_action)
end

if type(_G.keys) == "function" then
  local ok_keys, keys_err = pcall(_G.keys)
  if not ok_keys then
    io.stderr:write(keys_err .. "\n")
    os.exit(1)
  end
end

local function emit(key, value)
  io.write(key)
  io.write("=")
  io.write(tostring(value))
  io.write("\n")
end

local function expect_table(name, value)
  if value ~= nil and type(value) ~= "table" then
    io.stderr:write(name .. " must be a table\n")
    os.exit(1)
  end
end

local function emit_string(name, value)
  if value == nil then
    return
  end
  if type(value) ~= "string" then
    io.stderr:write(name .. " must be a string\n")
    os.exit(1)
  end
  emit(name, value)
end

local function emit_number(name, value)
  if value == nil then
    return
  end
  if type(value) ~= "number" then
    io.stderr:write(name .. " must be a number\n")
    os.exit(1)
  end
  emit(name, value)
end

local function emit_boolean(name, value)
  if value == nil then
    return
  end
  if type(value) ~= "boolean" then
    io.stderr:write(name .. " must be a boolean\n")
    os.exit(1)
  end
  emit(name, value)
end

local function emit_bool_like(name, value)
  if value == nil then
    return
  end
  if type(value) == "boolean" then
    emit(name, value)
    return
  end
  if type(value) == "number" then
    emit(name, value)
    return
  end
  io.stderr:write(name .. " must be a boolean or number\n")
  os.exit(1)
end

local function pick(primary, fallback)
  if primary ~= nil then
    return primary
  end
  return fallback
end

general = pick(cfg.general, _G.general) or {}

local function as_string(value)
  if value == nil then
    return nil
  end
  if type(value) ~= "string" then
    return tostring(value)
  end
  return value
end

local function render_keybind_entry(entry, index)
  if type(entry) == "string" then
    return entry
  end

  if type(entry) ~= "table" then
    io.stderr:write("keybindings[" .. tostring(index) .. "] must be string or table\n")
    os.exit(1)
  end

  local combo = as_string(pick(entry.combo, entry[1]))
  if (combo == nil or combo == "") and entry.mods and entry.key then
    combo = tostring(entry.mods):gsub("%s+", "+") .. "+" .. tostring(entry.key)
  end
  local action = as_string(pick(entry.action, entry[2]))
  local arg = as_string(pick(entry.arg, pick(entry.command, entry[3])))

  if combo == nil or combo == "" then
    io.stderr:write("keybindings[" .. tostring(index) .. "] missing combo\n")
    os.exit(1)
  end
  if action == nil or action == "" then
    io.stderr:write("keybindings[" .. tostring(index) .. "] missing action\n")
    os.exit(1)
  end

  if arg and arg ~= "" then
    return combo .. " " .. action .. " " .. arg
  end
  return combo .. " " .. action
end

emit_string("main_key", pick(general.main_key, pick(cfg.main_key, _G.main_key)))
emit_string("modkey", pick(general.modkey, pick(cfg.modkey, _G.modkey)))
emit_string("terminal", pick(general.terminal, pick(cfg.terminal, _G.terminal)))
emit_string("launcher", pick(general.launcher, pick(cfg.launcher, _G.launcher)))
emit_bool_like("focus_follow_mouse", pick(general.focus_follow_mouse, pick(cfg.focus_follow_mouse, _G.focus_follow_mouse)))
emit_bool_like("no_csd", pick(general.no_csd, pick(cfg.no_csd, _G.no_csd)))
emit_number("border_size", pick(general.border_size, pick(cfg.border_size, _G.border_size)))
emit_number("gap_size", pick(general.gap_size, pick(cfg.gap_size, _G.gap_size)))

local keybinds_table = pick(cfg.keybindings, pick(cfg.keybinds, pick(_G.keybindings, _G.keybinds)))
expect_table("keybindings", keybinds_table)
if keybinds_table then
  for index, bind in ipairs(keybinds_table) do
    emit("keybind." .. tostring(index), render_keybind_entry(bind, index))
  end
end

for index, bind in ipairs(raven_binds) do
  emit("keybind." .. tostring(index + 1000), bind)
end

local autostart = pick(cfg.autostart, _G.autostart)
expect_table("autostart", autostart)
if autostart then
  for index, command in ipairs(autostart) do
    if type(command) ~= "string" then
      io.stderr:write("autostart[" .. tostring(index) .. "] must be a string\n")
      os.exit(1)
    end
    emit("autostart." .. tostring(index), command)
  end
end

local window_rules = pick(cfg.window_rules, pick(cfg.rules, pick(_G.window_rules, _G.rules)))
expect_table("window_rules", window_rules)
if window_rules then
  local rule_index = 1

  local function emit_rule(rule, key_name)
    if type(rule) ~= "table" then
      io.stderr:write("window_rules entry must be a table\n")
      os.exit(1)
    end

    local prefix = "window_rule." .. tostring(rule_index) .. "."
    emit_string(prefix .. "class", pick(rule.class, key_name))
    emit_string(prefix .. "app_id", pick(rule.app_id, rule.appid))
    emit_string(prefix .. "title", rule.title)
    emit_string(prefix .. "workspace", pick(rule.workspace, rule.ws))
    emit_bool_like(prefix .. "floating", rule.floating)
    emit_bool_like(prefix .. "fullscreen", rule.fullscreen)
    emit_bool_like(prefix .. "focus", rule.focus)
    emit_number(prefix .. "width", rule.width)
    emit_number(prefix .. "height", rule.height)
    rule_index = rule_index + 1
  end

  for _, rule in ipairs(window_rules) do
    emit_rule(rule, nil)
  end

  for key, rule in pairs(window_rules) do
    if type(key) == "string" then
      emit_rule(rule, key)
    end
  end
end

local monitors = pick(cfg.monitors, _G.monitors)
expect_table("monitors", monitors)
if monitors then
  local function emit_monitor(index, monitor, key_name)
    if type(monitor) ~= "table" then
      io.stderr:write("monitors[" .. tostring(index) .. "] must be a table\n")
      os.exit(1)
    end

    local prefix = "monitor." .. tostring(index) .. "."
    emit_string(prefix .. "name", pick(monitor.name, pick(monitor.output, key_name)))

    local enabled = monitor.enabled
    if enabled == nil and monitor.off ~= nil then
      if type(monitor.off) ~= "boolean" then
        io.stderr:write("monitors[" .. tostring(index) .. "].off must be a boolean\n")
        os.exit(1)
      end
      enabled = not monitor.off
    end
    emit_bool_like(prefix .. "enabled", enabled)

    local position = monitor.position
    if position ~= nil and type(position) ~= "table" then
      io.stderr:write("monitors[" .. tostring(index) .. "].position must be a table\n")
      os.exit(1)
    end

    emit_string(prefix .. "mode", monitor.mode)
    emit_number(prefix .. "width", monitor.width)
    emit_number(prefix .. "height", monitor.height)
    emit_number(prefix .. "refresh_hz", pick(monitor.refresh_hz, pick(monitor.refresh, monitor.hz)))
    emit_number(prefix .. "x", pick(monitor.x, position and position.x or nil))
    emit_number(prefix .. "y", pick(monitor.y, position and position.y or nil))
    emit_number(prefix .. "scale", monitor.scale)
    emit_string(prefix .. "transform", monitor.transform)
  end

  local monitor_index = 1
  for _, monitor in ipairs(monitors) do
    emit_monitor(monitor_index, monitor, nil)
    monitor_index = monitor_index + 1
  end

  for key, monitor in pairs(monitors) do
    if type(key) == "string" then
      emit_monitor(monitor_index, monitor, key)
      monitor_index = monitor_index + 1
    end
  end
end

expect_table("layout", cfg.layout)
expect_table("gaps", cfg.gaps)
expect_table("cursor", cfg.cursor)
expect_table("wallpaper", cfg.wallpaper)
expect_table("xwayland", cfg.xwayland)

local layout = cfg.layout or {}
local gaps = pick(layout.gaps, cfg.gaps)
expect_table("layout.gaps", gaps)
gaps = gaps or {}

emit_number("master_factor", pick(layout.master_factor, cfg.master_factor))
emit_number("num_master", pick(layout.num_master, cfg.num_master))
emit_boolean("smart_gaps", pick(layout.smart_gaps, cfg.smart_gaps))

emit_number("gaps.outer_horizontal", gaps.outer_horizontal)
emit_number("gaps.outer_vertical", gaps.outer_vertical)
emit_number("gaps.inner_horizontal", gaps.inner_horizontal)
emit_number("gaps.inner_vertical", gaps.inner_vertical)

local cursor = cfg.cursor or {}
emit_string("cursor_theme", pick(cursor.theme, cfg.cursor_theme))
emit_number("cursor_size", pick(cursor.size, cfg.cursor_size))

local wallpaper = cfg.wallpaper or {}
emit_bool_like("wallpaper.enabled", pick(wallpaper.enabled, cfg.wallpaper_enabled))
emit_string("wallpaper.restore_command", pick(wallpaper.restore_command, pick(wallpaper.command, pick(cfg.wallpaper_restore_command, pick(cfg.wallpaper_command, pick(_G.wallpaper_restore_command, _G.wallpaper_command))))))
emit_string("wallpaper.image", pick(wallpaper.image, pick(wallpaper.path, cfg.wallpaper_image)))
emit_string("wallpaper.resize", pick(wallpaper.resize, cfg.wallpaper_resize))
emit_string("wallpaper.transition_type", pick(wallpaper.transition_type, cfg.wallpaper_transition_type))
emit_number("wallpaper.transition_duration", pick(wallpaper.transition_duration, cfg.wallpaper_transition_duration))

local xwayland = cfg.xwayland or {}
local xwayland_enabled = pick(xwayland.enabled, pick(cfg.xwayland_enabled, _G.xwayland_enabled))
if xwayland_enabled == nil and xwayland.off ~= nil then
  if type(xwayland.off) ~= "boolean" then
    io.stderr:write("xwayland.off must be a boolean\n")
    os.exit(1)
  end
  xwayland_enabled = not xwayland.off
end
emit_bool_like("xwayland.enabled", xwayland_enabled)
emit_string("xwayland.path", pick(xwayland.path, pick(cfg.xwayland_path, _G.xwayland_path)))
emit_string("xwayland.display", pick(xwayland.display, pick(cfg.xwayland_display, _G.xwayland_display)))
"#
}
