//! Settings → Beta features → Send a report (the TV app's T-21): what a
//! tester's system is, as a GitHub issue their phone opens from a QR code.
//! Nothing is sent from here: the phone opens a draft the tester can edit, and
//! only submitting it publishes anything.
//!
//! Redacted as the fixtures are, identifiers only: no player or household id,
//! no address, no serial. Rooms keep their names, which is what makes a report
//! readable. A port of `BetaReportViewModel.kt`'s `betaReport`.

use std::sync::LazyLock;

use regex::Regex;

/// Where reports go: a public repository, which the screen says beside the code.
pub const ISSUES: &str = "https://github.com/rahga/x2rock/issues/new";

/// Past this a QR code is too dense to scan off a screen across a room; byte
/// mode at the lowest correction holds 2953, and a phone needs margin below that.
pub const URL_LIMIT: usize = 2000;

/// The capability a line-in reports: always shown, since it is what a report
/// is most often for.
pub const LINE_IN: &str = "LINE_IN";

/// One speaker, as the report lists it.
#[derive(Clone, Debug, Default)]
pub struct Speaker {
    pub room: String,
    /// "One SL 18.8 (97.1-80312)"; `None` when it did not answer.
    pub what: Option<String>,
    pub capabilities: Vec<String>,
}

/// What the report is made from.
#[derive(Clone, Debug, Default)]
pub struct Input {
    pub app: String,
    pub device: String,
    /// The connection's own words, when it is not connected.
    pub connection: Option<String>,
    /// The speakers refused this app: Sonos's Authentication switch is on.
    pub authentication_required: bool,
    /// The last read of the household's stored sign-ins: how many, or why none.
    pub sign_ins: Option<Result<usize, String>>,
    pub speakers: Vec<Speaker>,
    /// "Kitchen: PLAYBACK_STATE_IDLE · container type …", one a room with a line-in.
    pub line_in_rooms: Vec<String>,
    pub refusal: Option<String>,
}

/// The issue's text, as the screen shows it, and the link that opens it as a draft.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub text: String,
    pub url: String,
}

pub fn report(input: &Input) -> Report {
    let speaker_lines = |with_capabilities: bool| -> Vec<String> {
        input
            .speakers
            .iter()
            .map(|s| {
                let what = s.what.as_deref().unwrap_or("didn't answer");
                let mut shown: Vec<&str> = s
                    .capabilities
                    .iter()
                    .map(String::as_str)
                    .filter(|c| with_capabilities || *c == LINE_IN)
                    .collect();
                shown.sort_unstable();
                let caps = if shown.is_empty() {
                    String::new()
                } else {
                    format!(" \u{00B7} {}", shown.join(","))
                };
                format!("- {}: {what}{caps}", s.room)
            })
            .collect()
    };
    let text = |with_capabilities: bool| -> String {
        let mut t = String::new();
        t.push_str("**What I tried, and what happened:**\n\n\n---\n");
        t.push_str(&format!("{} on {}\n", input.app, input.device));
        if input.authentication_required {
            t.push_str("\n**Authentication is on:** the speakers refused this app (ERROR_NO_PERMISSION).\n");
        } else if let Some(c) = &input.connection {
            t.push_str(&format!("\n**Connection:** {}\n", redact(c)));
        }
        // Said above the speakers, where cutting the end to fit cannot take it.
        match &input.sign_ins {
            Some(Ok(n)) => t.push_str(&format!("\n**Service sign-ins:** {n} read\n")),
            Some(Err(why)) => t.push_str(&format!(
                "\n**Service sign-ins:** none usable, {}\n",
                redact(why)
            )),
            None => {}
        }
        t.push_str("\n**Speakers**\n");
        if !with_capabilities {
            t.push_str("(capabilities left out to fit, but a line-in)\n");
        }
        for line in speaker_lines(with_capabilities) {
            t.push_str(&line);
            t.push('\n');
        }
        if !input.line_in_rooms.is_empty() {
            t.push_str("\n**Rooms with a line-in, now**\n");
            for line in &input.line_in_rooms {
                t.push_str(&format!("- {}\n", redact(line)));
            }
        }
        if let Some(r) = &input.refusal {
            t.push_str(&format!("\n**Last refusal:** {}\n", redact(r)));
        }
        t.trim_end().to_owned()
    };
    let url = |body: &str| {
        format!(
            "{ISSUES}?title={}&body={}",
            query_encode(&format!("Beta report: {}", input.app)),
            query_encode(body)
        )
    };

    let mut body = text(true);
    if url(&body).len() > URL_LIMIT {
        body = text(false);
    }
    while url(&body).len() > URL_LIMIT {
        let keep = body.chars().count() * 9 / 10;
        body = format!(
            "{}\n(cut to fit)",
            body.chars().take(keep).collect::<String>().trim_end()
        );
    }
    let url = url(&body);
    Report { text: body, url }
}

/// Percent-encodes a query value, leaving what a query may carry as it is (RFC
/// 3986): every byte encoded tripled the length, and length is what the QR code
/// pays for.
pub fn query_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        let c = b as char;
        if b.is_ascii_alphanumeric() || "-._~!$'()*,;:@/?".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

static REDACTIONS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"(?i)https?://\S+", "<link>"),
        (r"(?i)RINCON_[0-9A-F]{12,}(?:[_:][0-9A-Za-z]+)*", "<player>"),
        (r"Sonos_[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)?", "<household>"),
        (r"(?i)sonos-[0-9A-F]{12}(?:\.local)?", "<speaker>"),
        (r"(?i)\b(?:[0-9A-F]{2}[:-]){5}[0-9A-F]{2}\b", "<mac>"),
        (r"\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?\b", "<address>"),
        (
            r"(?i)\[[0-9A-F:]+\](?::\d+)?|(?:\b[0-9A-F]{1,4}:)*\b[0-9A-F]{1,4}::[0-9A-F]{0,4}(?::[0-9A-F]{1,4})*|::[0-9A-F]{1,4}(?::[0-9A-F]{1,4})*|\b(?:[0-9A-F]{1,4}:){7}[0-9A-F]{1,4}\b",
            "<address>",
        ),
    ]
    .into_iter()
    .map(|(pattern, mark)| (Regex::new(pattern).expect("a redaction pattern"), mark))
    .collect()
});

/// Free text with every identifier the report promises to leave out replaced,
/// for the fields whose words come from elsewhere: an error's message, a read's
/// reason. Room names are not touched.
pub fn redact(text: &str) -> String {
    REDACTIONS.iter().fold(text.to_owned(), |t, (re, mark)| {
        re.replace_all(&t, *mark).into_owned()
    })
}

/// What this runs on, in a few words: "SteamOS 3.7.13 (x86_64), Steam Deck".
pub fn device() -> String {
    let os = os_name().unwrap_or_else(|| std::env::consts::OS.to_owned());
    let mut device = format!("{os} ({})", std::env::consts::ARCH);
    if std::env::var_os("SteamDeck").is_some_and(|v| v == "1") {
        device.push_str(", Steam Deck");
    }
    device
}

#[cfg(target_os = "linux")]
fn os_name() -> Option<String> {
    let release = std::fs::read_to_string("/etc/os-release").ok()?;
    let line = release.lines().find(|l| l.starts_with("PRETTY_NAME="))?;
    Some(
        line.trim_start_matches("PRETTY_NAME=")
            .trim_matches('"')
            .to_owned(),
    )
}

#[cfg(not(target_os = "linux"))]
fn os_name() -> Option<String> {
    Some(match std::env::consts::OS {
        "windows" => "Windows".to_owned(),
        "macos" => "macOS".to_owned(),
        other => other.to_owned(),
    })
}

/// The white margin a reader needs round a code, in modules.
const QUIET: usize = 4;

/// Pixels a module: whole, so every module is drawn the same, and enough that
/// the screen only ever scales the image down.
const MODULE: usize = 6;

/// `text` as a QR code: RGBA pixels, black on white with its quiet zone, and
/// the side's length.
pub fn qr_pixels(text: &str) -> Option<(u32, Vec<u8>)> {
    let code =
        qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L).ok()?;
    let modules = code.width();
    let colors = code.to_colors();
    let side = (modules + 2 * QUIET) * MODULE;
    let mut pixels = vec![255u8; side * side * 4];
    for (i, color) in colors.iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let (mx, my) = (i % modules + QUIET, i / modules + QUIET);
        for y in my * MODULE..(my + 1) * MODULE {
            let row = y * side * 4;
            for x in mx * MODULE..(mx + 1) * MODULE {
                pixels[row + x * 4..row + x * 4 + 3].fill(0);
            }
        }
    }
    Some((side as u32, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(s: &str) -> String {
        let mut bytes = Vec::new();
        let raw = s.as_bytes();
        let mut i = 0;
        while i < raw.len() {
            if raw[i] == b'%' {
                bytes.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                bytes.push(raw[i]);
                i += 1;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    fn input() -> Input {
        Input {
            app: "x2rock-deck 0.1.0".into(),
            device: "SteamOS (x86_64), Steam Deck".into(),
            speakers: vec![Speaker {
                room: "Kitchen".into(),
                what: Some("One SL 18.8 (97.1-80312)".into()),
                capabilities: vec!["PLAYBACK".into(), "CLOUD".into()],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn it_says_what_each_speaker_is() {
        let r = report(&Input {
            refusal: Some("Line-In (Kitchen): ERROR_NOT_CAPABLE".into()),
            ..input()
        });
        assert!(
            r.text
                .contains("- Kitchen: One SL 18.8 (97.1-80312) \u{00B7} CLOUD,PLAYBACK"),
            "{}",
            r.text
        );
        assert!(
            r.text
                .contains("**Last refusal:** Line-In (Kitchen): ERROR_NOT_CAPABLE"),
            "{}",
            r.text
        );
        assert!(
            !r.text.contains("**Connection:**"),
            "a working connection has no connection line"
        );
    }

    /// The issue opens holding exactly what the screen showed, even where the
    /// text holds what a query means something by: "Rock & Roll" would end the
    /// body at its `&`.
    #[test]
    fn the_link_carries_the_text_as_shown() {
        let r = report(&Input {
            refusal: Some("Rock & Roll #2: a+b=c 100% \u{00FC}".into()),
            ..input()
        });
        assert!(r.url.starts_with(&format!("{ISSUES}?title=")));
        let query = r.url.split_once('?').unwrap().1;
        let parts: Vec<(&str, String)> = query
            .split('&')
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap();
                (k, decode(v))
            })
            .collect();
        assert_eq!(
            parts.iter().map(|p| p.0).collect::<Vec<_>>(),
            ["title", "body"]
        );
        assert_eq!(parts[1].1, r.text);
    }

    #[test]
    fn a_connection_failure_is_reported_in_its_own_words_redacted() {
        let r = report(&Input {
            connection: Some("no reply from sonos-48A6B81853E0.local within 5000ms".into()),
            ..input()
        });
        assert!(
            r.text
                .contains("**Connection:** no reply from <speaker> within 5000ms"),
            "{}",
            r.text
        );
    }

    #[test]
    fn it_says_whether_the_service_sign_ins_could_be_read() {
        let good = report(&Input {
            sign_ins: Some(Ok(4)),
            ..input()
        });
        assert!(good.text.contains("**Service sign-ins:** 4 read"));
        let bad = report(&Input {
            sign_ins: Some(Err("they could not be read (integrity check failed)".into())),
            ..input()
        });
        assert!(bad.text.contains(
            "**Service sign-ins:** none usable, they could not be read (integrity check failed)"
        ));
        assert!(
            bad.text.find("Service sign-ins") < bad.text.find("**Speakers**"),
            "above the speakers, out of the cut's reach"
        );
    }

    #[test]
    fn identifiers_in_free_text_are_redacted_from_the_report_and_the_link() {
        let refusal = "Line-In (Kitchen): failed to connect to sonos-48A6B818D138.local/192.168.86.26 (port 1443) \
            from /fe80::1c2b:3aff:fe4d:5e6f%wlan0 for RINCON_48A6B818D13801400:1546804781 in \
            Sonos_BgzkDDCeWajFguqqdHEXzFKe3x.Zv1xanSF at http://192.168.86.26:1400/x?token=abc, mac 34-7E-5C-31-AF-74";
        let r = report(&Input {
            refusal: Some(refusal.into()),
            sign_ins: Some(Err(
                "they could not be read (no reply from 10.0.0.7 / RINCON_542A1B83318001400)".into(),
            )),
            ..input()
        });
        let draft = decode(r.url.split_once("body=").unwrap().1);
        for leak in [
            "48A6B818D138",
            "192.168.86.26",
            "10.0.0.7",
            "fe80:",
            "RINCON_",
            "Sonos_Bgzk",
            "token=abc",
            "34-7E-5C",
        ] {
            assert!(
                !r.text.to_lowercase().contains(&leak.to_lowercase()),
                "{leak} in the shown report: {}",
                r.text
            );
            assert!(
                !draft.to_lowercase().contains(&leak.to_lowercase()),
                "{leak} in the issue draft"
            );
        }
        assert!(
            r.text.contains("Line-In (Kitchen)"),
            "the room name stays: {}",
            r.text
        );
    }

    #[test]
    fn clock_times_are_kept_while_ipv6_addresses_are_redacted() {
        let msg = "Kitchen already has an alarm at 06:45:00; connect to fe80::1, [::1]:1400, or 2001:db8::";
        let out = redact(msg);
        assert!(out.contains("06:45:00"), "clock time preserved: {out}");
        assert!(!out.contains("fe80:"), "fe80::1 redacted: {out}");
        assert!(!out.contains("::1"), "[::1]:1400 redacted: {out}");
        assert!(!out.contains("2001:db8"), "2001:db8:: redacted: {out}");
        assert_eq!(
            out,
            "Kitchen already has an alarm at 06:45:00; connect to <address>, <address>, or <address>"
        );
    }

    #[test]
    fn a_household_that_refuses_this_app_says_so() {
        assert!(
            report(&Input {
                authentication_required: true,
                ..input()
            })
            .text
            .contains("**Authentication is on:**")
        );
        assert!(!report(&input()).text.contains("Authentication is on"));
    }

    #[test]
    fn a_room_with_a_line_in_says_what_it_reports() {
        let mut i = input();
        i.speakers[0].capabilities.push(LINE_IN.into());
        i.line_in_rooms = vec!["Kitchen: PLAYBACK_STATE_IDLE \u{00B7} container type none".into()];
        let text = report(&i).text;
        assert!(
            text.contains("**Rooms with a line-in, now**\n- Kitchen: "),
            "{text}"
        );
        assert!(text.contains("LINE_IN"), "{text}");
    }

    /// Too big for one code: capabilities go first, then its end, and it says so.
    #[test]
    fn a_large_household_is_cut_to_fit_the_code() {
        let speaker = input().speakers[0].clone();
        let many = (1..=40)
            .map(|n| Speaker {
                room: format!("Room {n}"),
                capabilities: vec![
                    "PLAYBACK".into(),
                    "CLOUD".into(),
                    "AIRPLAY".into(),
                    "VOICE".into(),
                ],
                ..speaker.clone()
            })
            .collect();
        let r = report(&Input {
            speakers: many,
            ..input()
        });
        assert!(r.url.len() <= URL_LIMIT, "{}", r.url.len());
        assert!(
            r.text
                .contains("(capabilities left out to fit, but a line-in)"),
            "{}",
            r.text
        );
        assert!(r.text.ends_with("(cut to fit)"), "{}", r.text);
    }

    #[test]
    fn the_code_holds_a_full_length_link() {
        let (side, pixels) = qr_pixels(&"x".repeat(URL_LIMIT)).expect("a code");
        assert_eq!(pixels.len(), (side * side * 4) as usize);
        assert_eq!(side as usize % MODULE, 0, "whole modules");
        // The quiet zone is white all round: the first QUIET modules of the top row.
        assert!(
            pixels[..side as usize * 4 * QUIET * MODULE]
                .iter()
                .all(|v| *v == 255),
            "a quiet zone above"
        );
        assert!(pixels.chunks(4).any(|p| p[0] == 0), "something drawn");
    }
}
