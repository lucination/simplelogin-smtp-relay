//! Header address list parsing/replacement, mirroring server.py's
//! RelayHandler._replace_addresses (which wraps email.utils.getaddresses /
//! formataddr) and the Bcc-stripping behavior in _process().

use mailparse::addrparse;
use std::collections::HashMap;

/// A single mailbox from a To/Cc header: (display_name, address).
/// display_name is "" when there was none (mirrors email.utils.parseaddr
/// returning "" for missing display name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    pub display_name: String,
    pub address: String,
}

/// Parse a header value ("To"/"Cc" contents) into a flat list of mailboxes,
/// expanding groups the way Python's email.utils.getaddresses does
/// (it silently flattens groups to their member addresses).
pub fn parse_addresses(header_value: &str) -> Vec<Mailbox> {
    // For ordinary RFC 5322 mailbox lists, retain Python getaddresses' flat
    // semantics without mailparse's stricter recovery behavior. This covers
    // the relay's actual To/Cc replacement surface (including display names).
    if !header_value.contains(':') {
        return header_value
            .split(',')
            .filter_map(|part| {
                let part = part.trim();
                if part.is_empty() {
                    return None;
                }
                if let Some(start) = part.rfind('<') {
                    if let Some(end) = part[start + 1..].find('>') {
                        return Some(Mailbox {
                            display_name: part[..start].trim().trim_matches('"').to_string(),
                            address: part[start + 1..start + 1 + end].trim().to_string(),
                        });
                    }
                }
                Some(Mailbox {
                    display_name: String::new(),
                    address: part.to_string(),
                })
            })
            .collect();
    }
    let mut out = Vec::new();
    match addrparse(header_value) {
        Ok(list) => {
            for addr in list.into_inner() {
                collect_mailboxes(addr, &mut out);
            }
        }
        Err(_) => {
            // mailparse failed; Python's getaddresses is very lenient and
            // rarely errors. Fall back to a naive comma split so we degrade
            // gracefully rather than dropping the header entirely.
            for part in header_value.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                out.push(Mailbox {
                    display_name: String::new(),
                    address: part.to_string(),
                });
            }
        }
    }
    out
}

fn collect_mailboxes(addr: mailparse::MailAddr, out: &mut Vec<Mailbox>) {
    match addr {
        mailparse::MailAddr::Single(info) => {
            out.push(Mailbox {
                display_name: info.display_name.unwrap_or_default(),
                address: info.addr,
            });
        }
        mailparse::MailAddr::Group(group) => {
            for info in group.addrs {
                out.push(Mailbox {
                    display_name: info.display_name.unwrap_or_default(),
                    address: info.addr,
                });
            }
        }
    }
}

/// Format a single mailbox back to a header-safe string, mirroring Python's
/// email.utils.formataddr((display_name, addr)):
/// - no display name -> bare address
/// - display name present -> quoted "Name" <addr> if it needs quoting,
///   else Name <addr>. We always quote-if-needed for safety/parity with
///   Python's formataddr, which quotes when the name contains specials.
pub fn format_addr(display_name: &str, addr: &str) -> String {
    if display_name.is_empty() {
        return addr.to_string();
    }
    let needs_quoting = display_name.chars().any(|c| {
        matches!(
            c,
            '(' | ')' | '<' | '>' | '@' | ',' | ';' | ':' | '\\' | '"' | '.' | '[' | ']'
        )
    });
    if needs_quoting {
        let escaped = display_name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{}\" <{}>", escaped, addr)
    } else {
        format!("{} <{}>", display_name, addr)
    }
}

/// Mirrors RelayHandler._replace_addresses: for each address in the header,
/// if it's a key in alias_map, replace with the reverse alias's bare address
/// (parsed via parseaddr-equivalent) while KEEPING the original display
/// name; otherwise leave the mailbox untouched. Join with ", ".
pub fn replace_addresses(header_value: &str, alias_map: &HashMap<String, String>) -> String {
    let addresses = parse_addresses(header_value);
    let mut replaced = Vec::with_capacity(addresses.len());
    for mb in addresses {
        if let Some(reverse) = alias_map.get(&mb.address) {
            let (_, reverse_addr) = parse_single_addr(reverse);
            replaced.push(format_addr(&mb.display_name, &reverse_addr));
        } else {
            replaced.push(format_addr(&mb.display_name, &mb.address));
        }
    }
    replaced.join(", ")
}

/// Equivalent of email.utils.parseaddr for a single address-ish string
/// (used to pull the bare address out of a reverse_alias value, which
/// may itself be "Name <addr>" or just "addr").
pub fn parse_single_addr(value: &str) -> (String, String) {
    // mailparse's addrparse is intentionally strict around some display-name
    // forms accepted by Python's email.utils.parseaddr. Extract an explicit
    // angle-bracket address first, exactly the common parseaddr case.
    if let Some(start) = value.rfind('<') {
        if let Some(end) = value[start + 1..].find('>') {
            let display = value[..start].trim().trim_matches('"').to_string();
            return (
                display,
                value[start + 1..start + 1 + end].trim().to_string(),
            );
        }
    }
    match addrparse(value) {
        Ok(list) => {
            let inner = list.into_inner();
            if let Some(first) = inner.into_iter().next() {
                match first {
                    mailparse::MailAddr::Single(info) => {
                        return (info.display_name.unwrap_or_default(), info.addr);
                    }
                    mailparse::MailAddr::Group(group) => {
                        if let Some(info) = group.addrs.into_iter().next() {
                            return (info.display_name.unwrap_or_default(), info.addr);
                        }
                    }
                }
            }
            (String::new(), value.trim().to_string())
        }
        Err(_) => (String::new(), value.trim().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn single_to_replaced() {
        let m = map(&[("bob@example.com", "reverse1@sl.local")]);
        let out = replace_addresses("bob@example.com", &m);
        assert_eq!(out, "reverse1@sl.local");
    }

    #[test]
    fn multiple_to_all_replaced() {
        let m = map(&[
            ("bob@example.com", "reverse1@sl.local"),
            ("carol@example.com", "reverse2@sl.local"),
        ]);
        let out = replace_addresses("bob@example.com, carol@example.com", &m);
        assert_eq!(out, "reverse1@sl.local, reverse2@sl.local");
    }

    #[test]
    fn display_name_preserved() {
        let m = map(&[("bob@example.com", "reverse1@sl.local")]);
        let out = replace_addresses("Bob Smith <bob@example.com>", &m);
        assert_eq!(out, "Bob Smith <reverse1@sl.local>");
    }

    #[test]
    fn reverse_alias_with_its_own_display_name_uses_original_display_name() {
        // Python: reverse_addr comes from parseaddr(alias_map[addr]), but the
        // display name used in the output is the ORIGINAL header's display
        // name, not the reverse alias's display name.
        let m = map(&[("bob@example.com", "SimpleLogin <reverse1@sl.local>")]);
        let out = replace_addresses("Bob Smith <bob@example.com>", &m);
        assert_eq!(out, "Bob Smith <reverse1@sl.local>");
    }

    #[test]
    fn unmapped_address_left_alone() {
        let m = map(&[("bob@example.com", "reverse1@sl.local")]);
        let out = replace_addresses("bob@example.com, dave@example.com", &m);
        assert_eq!(out, "reverse1@sl.local, dave@example.com");
    }

    #[test]
    fn unmapped_with_display_name_left_alone() {
        let m: HashMap<String, String> = HashMap::new();
        let out = replace_addresses("Dave Jones <dave@example.com>", &m);
        assert_eq!(out, "Dave Jones <dave@example.com>");
    }

    #[test]
    fn parse_named_reverse_alias_to_bare_address() {
        assert_eq!(
            parse_single_addr("Reverse B <rev-b@simplelogin.test>").1,
            "rev-b@simplelogin.test"
        );
    }

    #[test]
    fn empty_header_yields_empty_string() {
        let m: HashMap<String, String> = HashMap::new();
        let out = replace_addresses("", &m);
        assert_eq!(out, "");
    }
}
