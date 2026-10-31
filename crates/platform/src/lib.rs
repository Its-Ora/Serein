//! Credential persistence and temporary owner-operated login/verification surfaces.
#[cfg(target_os = "macos")]
pub mod app_menu;
pub mod audio;
pub mod badge;
pub mod captcha;
pub mod compositor;
pub mod game_activity;
pub mod heic;
pub mod hotkeys;
mod login_keyring;
pub mod memory;
pub mod notifications;
pub mod plaintext_login;
pub mod pointer;
pub mod processes;
pub mod proxy_credentials;
pub mod save;
pub mod single_instance;
pub mod startup;
pub mod system_theme;
pub mod tray;
pub mod video;
#[cfg(target_os = "macos")]
pub mod window;
pub mod window_effects;
use client_core::auth::{Failure, SessionSecret};
pub use pointer::cursor_position;
#[cfg(not(target_os = "linux"))]
use std::{
	sync::{
		Arc,
		mpsc::{self, Receiver},
	},
	time::{Duration, Instant},
};
#[cfg(not(target_os = "linux"))]
use wry::{WebView, WebViewBuilder};
#[cfg(target_os = "linux")]
mod login_linux;
#[cfg(target_os = "linux")]
pub use login_linux::LoginView;

/// Logical height of the native header the desktop app draws above the login webview.
pub const LOGIN_HEADER_HEIGHT: f32 = 56.0;
const SERVICE: &str = "cz.viceverse.serein";
#[cfg(not(feature = "development-data"))]
const ACCOUNT: &str = "discord-session";
#[cfg(feature = "development-data")]
const ACCOUNT: &str = "discord-session.development";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialError {
	/// No OS credential store exists (e.g. Linux without a Secret Service provider).
	NoStore,
	Unavailable,
	Invalid,
	TimedOut,
}

#[cfg(target_os = "linux")]
pub(crate) fn ensure_gtk_application_id() {
	static INIT: std::sync::Once = std::sync::Once::new();
	INIT.call_once(|| {
		use gtk4::gio::prelude::ApplicationExt;
		let app = gtk4::gio::Application::new(Some(SERVICE), gtk4::gio::ApplicationFlags::empty());
		if let Err(error) = app.register(gtk4::gio::Cancellable::NONE) {
			eprintln!("Linux login/verification: GApplication registration failed: {error}");
		}
		std::mem::forget(app);
	});
}

/// Check GTK's effective encoding before WebKit starts Flatpak subprocesses.
#[cfg(target_os = "linux")]
pub(crate) fn ensure_webkit_locale() -> Result<(), &'static str> {
	let flatpak = std::path::Path::new("/.flatpak-info").is_file()
		|| std::env::var_os("FLATPAK_ID").is_some();
	if flatpak && !gtk4::glib::charset().0 {
		return Err(
			"Serein Flatpak login/verification requires a UTF-8 locale. Repair runtime languages and restart Serein; see Flatpak troubleshooting.",
		);
	}
	Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod flatpak_locale_tests {
	#[test]
	fn non_utf8_webkit_preflight_is_local_to_flatpak() {
		const CHILD: &str = "SEREIN_TEST_WEBKIT_LOCALE_CHILD";
		if let Some(mode) = std::env::var_os(CHILD) {
			// Fresh processes avoid GLib's cached encoding. Only the explicitly display-backed
			// regression initializes GTK; neither test constructs WebKit or authenticates.
			if std::env::var_os("SEREIN_TEST_WEBKIT_INITIALIZE_GTK").is_some() {
				gtk4::init().expect("display-backed GTK initialization");
			}
			assert_eq!(gtk4::glib::charset().0, mode == "utf8");
			assert_eq!(super::ensure_webkit_locale().is_err(), mode == "flatpak");
			assert_eq!(
				std::env::var_os("LC_ALL").as_deref(),
				Some(std::ffi::OsStr::new("C"))
			);
			return;
		}
		check_encoding_cases(false);
	}

	#[test]
	#[ignore = "requires a Linux display; explicitly exercised under Xvfb in native CI"]
	fn post_gtk_flatpak_encoding_preflight() {
		check_encoding_cases(true);
	}

	fn check_encoding_cases(initialize_gtk: bool) {
		const CHILD: &str = "SEREIN_TEST_WEBKIT_LOCALE_CHILD";
		for mode in ["native", "flatpak", "utf8"] {
			if mode == "native" && std::path::Path::new("/.flatpak-info").is_file() {
				continue; // A real sandbox marker cannot be removed to simulate a native app.
			}
			let mut child = std::process::Command::new(std::env::current_exe().unwrap());
			child
				.args([
					"--exact",
					"flatpak_locale_tests::non_utf8_webkit_preflight_is_local_to_flatpak",
				])
				.env(CHILD, mode)
				.env("LC_ALL", "C")
				.env("CHARSET", if mode == "utf8" { "UTF-8" } else { "US-ASCII" })
				.env_remove("FLATPAK_ID")
				.env_remove("SEREIN_TEST_WEBKIT_INITIALIZE_GTK");
			if initialize_gtk {
				child.env("SEREIN_TEST_WEBKIT_INITIALIZE_GTK", "1");
			}
			if mode != "native" {
				child.env("FLATPAK_ID", "cz.viceverse.serein");
			}
			let output = child.output().unwrap();
			assert!(
				output.status.success(),
				"{mode}: {}",
				String::from_utf8_lossy(&output.stderr)
			);
			assert!(
				String::from_utf8(output.stdout)
					.unwrap()
					.contains("running 1 test")
			);
		}
	}
}

/// Where a saved login ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Saved {
	CredentialStore,
	/// The owner opted into the plaintext fallback and no credential store accepted it.
	Plaintext,
}
/// The entry restored on launch. Switching accounts rewrites it from the per-account entry.
pub fn load_session() -> Result<Option<SessionSecret>, CredentialError> {
	load_login(ACCOUNT)
}
pub fn save_session(secret: &SessionSecret) -> Result<Saved, CredentialError> {
	save_login(ACCOUNT, secret)
}
pub fn forget_session() -> Result<(), CredentialError> {
	forget_login(ACCOUNT)
}
/// One entry per remembered account, so the switcher never keeps a second copy in memory.
fn account_entry(account: model::Id) -> String {
	format!("{ACCOUNT}.{account}")
}
pub fn load_account_session(account: model::Id) -> Result<Option<SessionSecret>, CredentialError> {
	load_login(&account_entry(account))
}
pub fn save_account_session(
	account: model::Id,
	secret: &SessionSecret,
) -> Result<Saved, CredentialError> {
	save_login(&account_entry(account), secret)
}
pub fn forget_account_session(account: model::Id) -> Result<(), CredentialError> {
	forget_login(&account_entry(account))
}
/// A fallback file is authoritative until a confirmed OS save removes it. A marker
/// remains after deletion to prevent an older (or late) keyring write restoring it.
fn load_login(name: &str) -> Result<Option<SessionSecret>, CredentialError> {
	let (directory, enabled) = plaintext_login::configuration();
	if let Some(directory) = directory {
		if enabled && let Some(secret) = plaintext_login::load(&directory, name)? {
			return Ok(Some(secret));
		}
		if plaintext_login::keyring_stale(&directory, name)? {
			return Ok(None);
		}
	}
	login_keyring::load(name)
}
fn save_login(name: &str, secret: &SessionSecret) -> Result<Saved, CredentialError> {
	let (directory, enabled) = plaintext_login::configuration();
	// Refresh any existing fallback before native IO: a cleanup error must never leave
	// an older plaintext token authoritative after a successful keyring write.
	if enabled
		&& let Some(directory) = directory.as_ref()
		&& plaintext_login::keyring_stale(directory, name)?
	{
		plaintext_login::save(directory, name, secret)?;
	}
	match login_keyring::save(name, secret) {
		Ok(()) => {
			if let Some(directory) = directory {
				plaintext_login::keyring_current(&directory, name)?;
			}
			Ok(Saved::CredentialStore)
		}
		Err(error) => {
			if enabled && let Some(directory) = directory {
				plaintext_login::save(&directory, name, secret)?;
				Ok(Saved::Plaintext)
			} else {
				Err(error)
			}
		}
	}
}
fn forget_login(name: &str) -> Result<(), CredentialError> {
	let (directory, _) = plaintext_login::configuration();
	// Fence even a late default-backend save before attempting native deletion. An
	// unavailable/full native queue must never resurrect a logged-out launch entry.
	let local = directory.as_ref().map_or(Ok(()), |directory| {
		plaintext_login::mark_stale(directory, name)?;
		plaintext_login::forget(directory, name)
	});
	let stored = login_keyring::forget(name);
	local?;
	// Forgotten roster slots can be released when no OS store exists. The launch
	// marker stays until confirmed deletion, preventing wrong-account restoration.
	if (stored.is_ok() || (stored == Err(CredentialError::NoStore) && name != ACCOUNT))
		&& let Some(directory) = directory
	{
		plaintext_login::keyring_current(&directory, name)?;
	}
	match stored {
		Err(CredentialError::NoStore) => Ok(()),
		stored => stored,
	}
}

fn entry(name: &str) -> Result<keyring_core::Entry, CredentialError> {
	credential_store()?;
	keyring_core::Entry::new(SERVICE, name).map_err(|error| match error {
		keyring_core::Error::NoDefaultStore => CredentialError::NoStore,
		_ => CredentialError::Unavailable,
	})
}
#[cfg(not(target_os = "linux"))]
fn credential_store() -> Result<(), CredentialError> {
	keyring::Entry::store_status()
		.as_ref()
		.copied()
		.map_err(|_| CredentialError::NoStore)
}
/// Unlike keyring's one-shot default, a missing Secret Service is retried, so a provider that
/// starts after Serein (or KWallet, which may need activating) is still found.
#[cfg(target_os = "linux")]
fn credential_store() -> Result<(), CredentialError> {
	const RETRY: std::time::Duration = std::time::Duration::from_secs(15);
	static LAST_FAILURE: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
	let mut last_failure = LAST_FAILURE
		.lock()
		.unwrap_or_else(std::sync::PoisonError::into_inner);
	if keyring_core::get_default_store().is_some() {
		return Ok(());
	}
	if last_failure.is_some_and(|at| at.elapsed() < RETRY) {
		return Err(CredentialError::NoStore);
	}
	let store = zbus_secret_service_keyring_store::Store::new().or_else(|_| {
		activate_kwallet();
		zbus_secret_service_keyring_store::Store::new()
	});
	match store {
		Ok(store) => {
			keyring_core::set_default_store(store);
			*last_failure = None;
			Ok(())
		}
		Err(_) => {
			*last_failure = Some(std::time::Instant::now());
			Err(CredentialError::NoStore)
		}
	}
}
/// KWallet serves the Secret Service API but, unlike GNOME Keyring, often is not D-Bus
/// activatable under that name, so outside a Plasma session nothing starts it. Start the
/// wallet daemon itself, then wait briefly for it to claim the Secret Service name.
#[cfg(target_os = "linux")]
fn activate_kwallet() {
	use zbus::names::WellKnownName;
	let Ok(connection) = zbus::blocking::Connection::session() else {
		return;
	};
	let Ok(bus) = zbus::blocking::fdo::DBusProxy::new(&connection) else {
		return;
	};
	let started = ["org.kde.kwalletd6", "org.kde.kwalletd5"]
		.into_iter()
		.any(|daemon| {
			bus.start_service_by_name(WellKnownName::from_static_str_unchecked(daemon), 0)
				.is_ok()
		});
	if !started {
		return;
	}
	let secrets = WellKnownName::from_static_str_unchecked("org.freedesktop.secrets");
	for _ in 0..20 {
		if bus.name_has_owner(secrets.clone().into()).unwrap_or(false) {
			return;
		}
		std::thread::sleep(std::time::Duration::from_millis(100));
	}
}
fn load_entry(name: &str) -> Result<Option<SessionSecret>, CredentialError> {
	match entry(name)?.get_password() {
		Ok(value) => SessionSecret::from_owner_input(value)
			.map(Some)
			.map_err(|_| CredentialError::Invalid),
		Err(keyring_core::Error::NoEntry) => Ok(None),
		Err(_) => Err(CredentialError::Unavailable),
	}
}
fn save_entry(name: &str, secret: &SessionSecret) -> Result<(), CredentialError> {
	entry(name)?
		.set_password(secret.expose())
		.map_err(|_| CredentialError::Unavailable)
}
fn forget_entry(name: &str) -> Result<(), CredentialError> {
	match entry(name)?.delete_credential() {
		Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
		Err(_) => Err(CredentialError::Unavailable),
	}
}
fn discord_origin(value: &str) -> bool {
	url::Url::parse(value).is_ok_and(|url| {
		url.scheme() == "https"
			&& url.host_str() == Some("discord.com")
			&& url.port_or_known_default() == Some(443)
			&& url.username().is_empty()
			&& url.password().is_none()
	})
}
#[cfg(any(not(target_os = "linux"), test))]
fn login_navigation(value: &str) -> bool {
	discord_origin(value) || captcha::hcaptcha_origin(value)
}
/// Fork: validates the identity headers Discord's web client sent with the login token.
/// Returns `None` unless the super-properties decode to a bounded JSON object with a
/// printable user agent; locale and timezone fall back to the defaults when malformed.
pub fn login_identity(
	super_properties: &str,
	locale: &str,
	timezone: &str,
) -> Option<client_core::fingerprint::Identity> {
	if super_properties.is_empty() || super_properties.len() > 4096 {
		return None;
	}
	let decoded = decode_base64(super_properties)?;
	let properties = String::from_utf8(decoded).ok()?;
	let value: serde_json::Value = serde_json::from_str(&properties).ok()?;
	let object = value.as_object()?;
	let text = |key: &str, max: usize| {
		object
			.get(key)
			.and_then(|value| value.as_str())
			.filter(|value| {
				!value.is_empty()
					&& value.len() <= max
					&& value.chars().all(|c| (' '..='~').contains(&c))
			})
			.map(str::to_owned)
	};
	let user_agent = text("browser_user_agent", 512)?;
	let browser = text("browser", 64)?;
	let browser_version = text("browser_version", 64).unwrap_or_default();
	object.get("client_build_number")?.as_u64()?;
	let short = |value: &str, max: usize, extra: &[char]| {
		(!value.is_empty()
			&& value.len() <= max
			&& value
				.chars()
				.all(|c| c.is_ascii_alphanumeric() || extra.contains(&c)))
		.then(|| value.to_owned())
	};
	Some(client_core::fingerprint::Identity {
		super_properties: super_properties.to_owned(),
		properties,
		user_agent,
		browser,
		browser_version,
		locale: short(locale, 16, &['-'])
			.unwrap_or_else(|| client_core::fingerprint::LOCALE.into()),
		timezone: short(timezone, 64, &['/', '_', '-', '+']).unwrap_or_else(|| "UTC".into()),
	})
}
fn decode_base64(input: &str) -> Option<Vec<u8>> {
	let mut out = Vec::with_capacity(input.len() / 4 * 3);
	let mut buffer = 0u32;
	let mut bits = 0;
	for byte in input.bytes() {
		let value = match byte {
			b'A'..=b'Z' => byte - b'A',
			b'a'..=b'z' => byte - b'a' + 26,
			b'0'..=b'9' => byte - b'0' + 52,
			b'+' | b'-' => 62,
			b'/' | b'_' => 63,
			b'=' => break,
			_ => return None,
		};
		buffer = (buffer << 6) | u32::from(value);
		bits += 6;
		if bits >= 8 {
			bits -= 8;
			out.push((buffer >> bits) as u8);
		}
	}
	Some(out)
}
/// Receives only the account token used by THIS ephemeral, owner-operated login page.
/// No browser-profile reads, password interception, console instructions, or QR exchange implementation.
#[cfg(not(target_os = "linux"))]
pub struct LoginView {
	view: WebView,
	tokens: Receiver<(SessionSecret, Option<client_core::fingerprint::Identity>)>,
	identity: std::cell::RefCell<Option<client_core::fingerprint::Identity>>,
	opened: Instant,
}
#[cfg(not(target_os = "linux"))]
impl LoginView {
	pub fn open(
		parent: Arc<winit::window::Window>,
		wake: impl Fn() + Send + Sync + 'static,
	) -> Result<Self, Failure> {
		let (send, tokens) = mpsc::sync_channel(1);
		let mut random = [0_u8; 32];
		getrandom::fill(&mut random).map_err(|_| Failure::Protocol)?;
		let capability = random
			.iter()
			.map(|byte| format!("{byte:02x}"))
			.collect::<String>()
			+ ":";
		let script = include_str!("login-handoff.js")
			.replace("__SEREIN_LOGIN_CAPABILITY__", &capability)
			.replace("__SEREIN_LOGIN_IDENTITY__", "1");
		let builder = WebViewBuilder::new()
			.with_url("https://discord.com/login")
			.with_incognito(true)
			.with_devtools(false)
			.with_initialization_script_for_main_only(script, true)
			.with_navigation_handler(|url| login_navigation(&url))
			.with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
			.with_download_started_handler(|_, _| false)
			.with_ipc_handler(move |request| {
				// Token (2048) + super-properties (4096) + locale + timezone + separators.
				if !discord_origin(&request.uri().to_string()) || request.body().len() > 6400 {
					return;
				}
				let body = zeroize::Zeroizing::new(request.into_body());
				let Some(value) = body.strip_prefix(&capability) else {
					return;
				};
				let mut parts = value.split('\n');
				let token = parts.next().unwrap_or_default();
				let identity = login_identity(
					parts.next().unwrap_or_default(),
					parts.next().unwrap_or_default(),
					parts.next().unwrap_or_default(),
				);
				if token.len() <= 2048
					&& let Ok(secret) = SessionSecret::from_owner_input(token.to_owned())
				{
					let _ = send.try_send((secret, identity));
					wake();
				}
			});
		let view = builder
			.with_bounds(bounds(&parent))
			.build_as_child(parent.as_ref())
			.map_err(|_| Failure::Protocol)?;
		Ok(Self {
			view,
			tokens,
			identity: std::cell::RefCell::new(None),
			opened: Instant::now(),
		})
	}
	pub fn token(&self) -> Option<SessionSecret> {
		let (secret, identity) = self.tokens.try_recv().ok()?;
		*self.identity.borrow_mut() = identity;
		Some(secret)
	}
	/// The web client identity that came with the last token, if it validated.
	pub fn identity(&self) -> Option<client_core::fingerprint::Identity> {
		self.identity.borrow().clone()
	}
	pub fn expired(&self) -> bool {
		self.opened.elapsed() > Duration::from_secs(600)
	}
	pub fn crashed(&self) -> bool {
		false
	}
	pub fn resize(&self, parent: &winit::window::Window) {
		let _ = self.view.set_bounds(bounds(parent));
	}
	pub fn pump(&self) {}
}
#[cfg(not(target_os = "linux"))]
fn bounds(parent: &winit::window::Window) -> wry::Rect {
	let size = parent.inner_size();
	let header = (LOGIN_HEADER_HEIGHT as f64 * parent.scale_factor()).round() as u32;
	wry::Rect {
		position: wry::dpi::PhysicalPosition::new(0, header as i32).into(),
		size: wry::dpi::PhysicalSize::new(size.width, size.height.saturating_sub(header)).into(),
	}
}
#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn credential_account_names_are_profile_scoped() {
		#[cfg(feature = "development-data")]
		assert_eq!(ACCOUNT, "discord-session.development");
		#[cfg(not(feature = "development-data"))]
		assert_eq!(ACCOUNT, "discord-session");
		assert_eq!(account_entry(model::Id(7)), format!("{ACCOUNT}.7"));
	}
	#[test]
	fn login_identity_keeps_valid_web_client_headers_only() {
		let properties = r#"{"os":"Windows","browser":"Edge","browser_user_agent":"Mozilla/5.0 (Windows NT 10.0; Win64; x64) Edg/156.0.0.0","browser_version":"156.0.0.0","client_build_number":512345}"#;
		let encoded = client_core::fingerprint::encode_base64(properties.as_bytes());
		let identity = login_identity(&encoded, "fr", "Europe/Paris").unwrap();
		assert_eq!(identity.properties, properties);
		assert_eq!(identity.browser, "Edge");
		assert!(identity.user_agent.ends_with("Edg/156.0.0.0"));
		assert_eq!(
			(identity.locale.as_str(), identity.timezone.as_str()),
			("fr", "Europe/Paris")
		);
		// Malformed locale/timezone fall back instead of reaching a header.
		let identity = login_identity(&encoded, "fr\r\nX: y", "../etc").unwrap();
		assert_eq!(identity.timezone, "UTC");
		assert_eq!(identity.locale, client_core::fingerprint::LOCALE);
		// No build number, not an object, not base64, or a control character in the agent.
		for bad in [
			client_core::fingerprint::encode_base64(
				br#"{"browser":"Edge","browser_user_agent":"x"}"#,
			),
			client_core::fingerprint::encode_base64(b"[1]"),
			"%%%".to_owned(),
			client_core::fingerprint::encode_base64(
				br#"{"browser":"Edge","browser_user_agent":"a\nb","client_build_number":1}"#,
			),
		] {
			assert!(login_identity(&bad, "fr", "UTC").is_none());
		}
	}
	#[test]
	fn handoff_accepts_only_our_discord_origin() {
		assert!(discord_origin("https://discord.com/login"));
		for value in [
			"http://discord.com",
			"https://discord.com.evil.test",
			"https://evil.test/discord.com",
			"https://user@discord.com",
			"https://discord.com:444",
		] {
			assert!(!discord_origin(value));
		}
		let script = include_str!("login-handoff.js");
		assert!(!script.contains("localStorage"));
		assert!(!script.contains("password"));

		{
			assert!(login_navigation("https://newassets.hcaptcha.com/captcha/"));
			assert!(!login_navigation("http://hcaptcha.com/"));
			assert!(!login_navigation("https://hcaptcha.com.evil.test/"));
		}
	}
}
