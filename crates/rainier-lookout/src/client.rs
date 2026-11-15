//! What made a request: the client's address, browser, operating system and
//! device, and the page it came from — [`Capture::client`](crate::config::Capture::client).
//!
//! Read from headers every request carries anyway, so it needs nothing from
//! the frontend, and works the same when the frontend is a separate
//! application: its browser calls this one directly, and this one sees the
//! browser's headers. The request's thread only copies a few header values;
//! parsing the user agent is the writer's work, for batches it keeps.
//!
//! The parsing is deliberately small: the browsers and systems people use,
//! read the way their own documentation says to read them. An agent it does
//! not recognise is kept as it came, so nothing is lost by a guess not made.

use std::net::IpAddr;

use serde_json::{json, Map, Value};

use crate::config::ClientIp;

/// The raw values, copied when the request starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClientRaw {
    pub(crate) ip: Option<IpAddr>,
    pub(crate) user_agent: Option<String>,
    /// `Sec-CH-UA-Platform`, `Sec-CH-UA-Platform-Version`, `Sec-CH-UA-Mobile`,
    /// `Sec-CH-UA-Model`: what a Chromium browser says about itself without
    /// the user agent's frozen values.
    pub(crate) hint_platform: Option<String>,
    pub(crate) hint_platform_version: Option<String>,
    pub(crate) hint_mobile: Option<String>,
    pub(crate) hint_model: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) referer: Option<String>,
    pub(crate) language: Option<String>,
}

impl ClientRaw {
    /// Copy what is needed from `request`.
    pub(crate) fn of(request: &rainier_http::Request) -> Self {
        let header = |name: &str| {
            request
                .header(name)
                .map(|v| v.chars().take(512).collect::<String>())
                .filter(|v| !v.is_empty())
        };
        Self {
            ip: request.ip(),
            user_agent: header("user-agent"),
            hint_platform: header("sec-ch-ua-platform"),
            hint_platform_version: header("sec-ch-ua-platform-version"),
            hint_mobile: header("sec-ch-ua-mobile"),
            hint_model: header("sec-ch-ua-model"),
            origin: header("origin"),
            referer: header("referer"),
            language: header("accept-language"),
        }
    }

    /// The `client` object a kept request carries.
    pub(crate) fn describe(&self, ip: ClientIp) -> Value {
        let mut out = Map::new();
        if let Some(address) = self.ip.and_then(|a| mask_ip(a, ip)) {
            out.insert("ip".into(), json!(address));
        }
        let agent = self.user_agent.as_deref().map(parse_user_agent).unwrap_or_default();
        let unquote =
            |v: &Option<String>| v.as_deref().map(|v| v.trim().trim_matches('"').to_string());

        let browser =
            agent.browser.map(|(name, version)| json!({ "name": name, "version": version }));
        if let Some(browser) = browser {
            out.insert("browser".into(), browser);
        }
        // Client hints, when sent, are what the browser means; the user agent
        // freezes its operating system version (macOS stays at 10.15.7).
        let os_name = unquote(&self.hint_platform)
            .filter(|p| !p.is_empty())
            .or(agent.os.as_ref().map(|o| o.0.clone()));
        let os_version = unquote(&self.hint_platform_version)
            .filter(|v| !v.is_empty())
            .or(agent.os.as_ref().and_then(|o| o.1.clone()));
        if let Some(name) = os_name {
            out.insert("os".into(), json!({ "name": name, "version": os_version }));
        }
        let device = match self.hint_mobile.as_deref().map(str::trim) {
            Some("?1") if agent.device != Device::Tablet && agent.device != Device::Bot => {
                Device::Mobile
            }
            _ => agent.device,
        };
        out.insert("device".into(), json!(device.as_str()));
        if let Some(model) = unquote(&self.hint_model).filter(|m| !m.is_empty()) {
            out.insert("model".into(), json!(model));
        }
        if let Some(ua) = &self.user_agent {
            out.insert("user_agent".into(), json!(ua));
        }
        if let Some(origin) = &self.origin {
            out.insert("origin".into(), json!(origin));
        }
        if let Some(referer) = self.referer.as_deref().map(without_query) {
            out.insert("referer".into(), json!(referer));
        }
        if let Some(language) = self.language.as_deref().and_then(first_language) {
            out.insert("language".into(), json!(language));
        }
        Value::Object(out)
    }
}

/// An address cut to what [`ClientIp`] allows.
pub fn mask_ip(address: IpAddr, mode: ClientIp) -> Option<String> {
    match mode {
        ClientIp::None => None,
        ClientIp::Full => Some(address.to_string()),
        ClientIp::Partial => Some(match address {
            IpAddr::V4(v4) => {
                let [a, b, c, _] = v4.octets();
                format!("{a}.{b}.{c}.0/24")
            }
            IpAddr::V6(v6) => {
                if let Some(v4) = v6.to_ipv4_mapped() {
                    return mask_ip(IpAddr::V4(v4), mode);
                }
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2])
            }
        }),
    }
}

/// A URL without its query and fragment: a page's address, not the tokens a
/// link can carry in them.
fn without_query(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or(url).to_string()
}

/// The first language of an `Accept-Language`, without its weight.
fn first_language(header: &str) -> Option<String> {
    header
        .split(',')
        .next()
        .map(|tag| tag.split(';').next().unwrap_or(tag).trim().to_string())
        .filter(|tag| !tag.is_empty() && tag != "*")
}

/// What kind of device a request came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Device {
    /// A desktop or laptop browser — and an iPad asking for the desktop site,
    /// which says it is a Mac.
    Desktop,
    /// A phone.
    Mobile,
    /// A tablet.
    Tablet,
    /// A crawler, a monitor, or a command-line client.
    Bot,
    /// No user agent to go on.
    #[default]
    Unknown,
}

impl Device {
    /// The name the dashboard shows.
    pub fn as_str(self) -> &'static str {
        match self {
            Device::Desktop => "desktop",
            Device::Mobile => "mobile",
            Device::Tablet => "tablet",
            Device::Bot => "bot",
            Device::Unknown => "unknown",
        }
    }
}

/// A user agent, read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Agent {
    /// Name and `major.minor` version.
    pub browser: Option<(String, String)>,
    /// Name and, where the agent gives one, version.
    pub os: Option<(String, Option<String>)>,
    /// What kind of device.
    pub device: Device,
}

/// Read a `User-Agent`.
pub fn parse_user_agent(ua: &str) -> Agent {
    let lower = ua.to_ascii_lowercase();
    let bot = [
        "bot",
        "crawler",
        "spider",
        "slurp",
        "curl/",
        "wget/",
        "python-requests",
        "go-http-client",
        "headless",
        "monitor",
        "uptime",
        "playwright",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    let browser = browser_of(ua).or_else(|| {
        bot.then(|| {
            // A bot's own name: the first product token.
            let token = ua.split_whitespace().next().unwrap_or(ua);
            let (name, version) = token.split_once('/').unwrap_or((token, ""));
            (name.to_string(), short_version(version))
        })
    });
    let os = os_of(ua);

    let device = if bot {
        Device::Bot
    } else if ua.contains("iPad")
        || ua.contains("Tablet")
        || (ua.contains("Android") && !ua.contains("Mobile"))
    {
        Device::Tablet
    } else if ua.contains("Mobile") || ua.contains("iPhone") || ua.contains("iPod") {
        Device::Mobile
    } else if ua.is_empty() {
        Device::Unknown
    } else {
        Device::Desktop
    };

    Agent { browser, os, device }
}

/// The version after `token/`, as `major.minor`.
fn after(ua: &str, token: &str) -> Option<String> {
    let start = ua.find(token)? + token.len();
    let rest = &ua[start..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
    let version = &rest[..end];
    (!version.is_empty()).then(|| short_version(version))
}

fn short_version(version: &str) -> String {
    let mut parts = version.split('.').filter(|p| !p.is_empty());
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) => format!("{major}.{minor}"),
        (Some(major), None) => major.to_string(),
        _ => String::new(),
    }
}

fn browser_of(ua: &str) -> Option<(String, String)> {
    // Order matters: every Chromium browser also says Chrome and Safari, and
    // Chrome says Safari. The most specific token wins.
    let named = |name: &str, token: &str| after(ua, token).map(|v| (name.to_string(), v));
    named("Edge", "Edg/")
        .or_else(|| named("Edge", "EdgA/"))
        .or_else(|| named("Edge", "EdgiOS/"))
        .or_else(|| named("Opera", "OPR/"))
        .or_else(|| named("Samsung Internet", "SamsungBrowser/"))
        .or_else(|| named("Firefox", "FxiOS/"))
        .or_else(|| named("Firefox", "Firefox/"))
        .or_else(|| named("Chrome", "CriOS/"))
        .or_else(|| named("Chromium", "Chromium/"))
        .or_else(|| named("Chrome", "Chrome/"))
        .or_else(|| {
            // Safari's own version is `Version/`, not `Safari/` (a WebKit build).
            ua.contains("Safari/").then(|| named("Safari", "Version/")).flatten()
        })
}

fn os_of(ua: &str) -> Option<(String, Option<String>)> {
    let version_after = |token: &str| {
        let start = ua.find(token)? + token.len();
        let rest = &ua[start..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '_' || c == '.'))
            .unwrap_or(rest.len());
        let v = rest[..end].replace('_', ".");
        (!v.is_empty()).then_some(v)
    };
    if ua.contains("iPhone") || ua.contains("iPad") || ua.contains("iPod") {
        return Some(("iOS".into(), version_after("OS ")));
    }
    if ua.contains("Android") {
        return Some(("Android".into(), version_after("Android ")));
    }
    if ua.contains("Windows") {
        let version = version_after("Windows NT ").map(|nt| {
            match nt.as_str() {
                // Windows 11 still says 10.0; only client hints tell them apart.
                "10.0" => "10 or 11".to_string(),
                "6.3" => "8.1".to_string(),
                "6.2" => "8".to_string(),
                "6.1" => "7".to_string(),
                other => format!("NT {other}"),
            }
        });
        return Some(("Windows".into(), version));
    }
    if ua.contains("CrOS") {
        return Some(("ChromeOS".into(), None));
    }
    if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        return Some(("macOS".into(), version_after("Mac OS X ")));
    }
    if ua.contains("Linux") {
        return Some(("Linux".into(), None));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(ua: &str) -> (Option<(&str, &str)>, Option<&str>, Device) {
        // Leaked for the test's convenience: a borrowed view of the result.
        let agent: &'static Agent = Box::leak(Box::new(parse_user_agent(ua)));
        (
            agent.browser.as_ref().map(|(n, v)| (n.as_str(), v.as_str())),
            agent.os.as_ref().map(|(n, _)| n.as_str()),
            agent.device,
        )
    }

    #[test]
    fn reads_the_browsers_people_use() {
        let chrome = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
        assert_eq!(read(chrome), (Some(("Chrome", "131.0")), Some("Windows"), Device::Desktop));

        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.2903.70";
        assert_eq!(read(edge).0, Some(("Edge", "131.0")));

        let safari = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15";
        assert_eq!(read(safari), (Some(("Safari", "18.1")), Some("macOS"), Device::Desktop));

        let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1";
        assert_eq!(read(iphone), (Some(("Safari", "18.1")), Some("iOS"), Device::Mobile));
        assert_eq!(parse_user_agent(iphone).os.unwrap().1.as_deref(), Some("18.1"));

        let firefox = "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0";
        assert_eq!(read(firefox), (Some(("Firefox", "133.0")), Some("Linux"), Device::Desktop));

        let samsung = "Mozilla/5.0 (Linux; Android 14; SM-S918B) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/26.0 Chrome/122.0.0.0 Mobile Safari/537.36";
        assert_eq!(
            read(samsung),
            (Some(("Samsung Internet", "26.0")), Some("Android"), Device::Mobile)
        );

        let tablet = "Mozilla/5.0 (Linux; Android 13; SM-X710) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
        assert_eq!(read(tablet).2, Device::Tablet);

        let chrome_ios = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/131.0.6778.73 Mobile/15E148 Safari/604.1";
        assert_eq!(read(chrome_ios).0, Some(("Chrome", "131.0")));
    }

    #[test]
    fn a_bot_is_a_bot_and_keeps_its_name() {
        assert_eq!(read("curl/8.7.1"), (Some(("curl", "8.7")), None, Device::Bot));
        let google = "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";
        assert_eq!(read(google).2, Device::Bot);
        let headless = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/131.0.0.0 Safari/537.36";
        assert_eq!(read(headless).2, Device::Bot, "a test browser is not a person");
    }

    #[test]
    fn an_unknown_agent_is_not_guessed_at() {
        assert_eq!(read("SomethingNew/1.0"), (None, None, Device::Desktop));
        assert_eq!(read(""), (None, None, Device::Unknown));
    }

    #[test]
    fn a_partial_address_keeps_the_network_only() {
        let v4: IpAddr = "203.0.113.77".parse().unwrap();
        assert_eq!(mask_ip(v4, ClientIp::Partial).as_deref(), Some("203.0.113.0/24"));
        assert_eq!(mask_ip(v4, ClientIp::Full).as_deref(), Some("203.0.113.77"));
        assert_eq!(mask_ip(v4, ClientIp::None), None);
        let v6: IpAddr = "2001:db8:1:2:3:4:5:6".parse().unwrap();
        assert_eq!(mask_ip(v6, ClientIp::Partial).as_deref(), Some("2001:db8:1::/48"));
        let mapped: IpAddr = "::ffff:198.51.100.9".parse().unwrap();
        assert_eq!(mask_ip(mapped, ClientIp::Partial).as_deref(), Some("198.51.100.0/24"));
    }

    #[test]
    fn hints_win_over_the_frozen_user_agent_and_a_referer_loses_its_query() {
        let raw = ClientRaw {
            ip: Some("203.0.113.77".parse().unwrap()),
            user_agent: Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36".into()),
            hint_platform: Some("\"macOS\"".into()),
            hint_platform_version: Some("\"15.1.0\"".into()),
            hint_mobile: Some("?0".into()),
            hint_model: None,
            origin: Some("https://app.example.com".into()),
            referer: Some("https://app.example.com/reset?token=abc#x".into()),
            language: Some("en-GB,en;q=0.9".into()),
        };
        let client = raw.describe(ClientIp::Partial);
        assert_eq!(client["ip"], "203.0.113.0/24");
        assert_eq!(client["os"], json!({ "name": "macOS", "version": "15.1.0" }));
        assert_eq!(client["browser"]["name"], "Chrome");
        assert_eq!(client["device"], "desktop");
        assert_eq!(client["referer"], "https://app.example.com/reset", "no token in the query");
        assert_eq!(client["language"], "en-GB");
        assert_eq!(client["origin"], "https://app.example.com");
    }
}
