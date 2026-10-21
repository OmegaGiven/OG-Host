//! Compose variable interpolation: `$VAR`, `${VAR}`, `${VAR:-default}`, `${VAR-default}`,
//! `${VAR:?error}`, `${VAR?error}`, `${VAR:+alt}`, `${VAR+alt}`, and `$$` for a literal `$`.

use std::collections::HashMap;

use crate::{Error, Result};

pub type Vars = HashMap<String, String>;

/// Parse a `.env` file: `KEY=value`, `export KEY=value`, `#` comments, optional quotes.
pub fn parse_env_file(text: &str) -> Vars {
    let mut out = Vars::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let l = l.strip_prefix("export ").unwrap_or(l);
        let Some((k, v)) = l.split_once('=') else { continue };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        let v = v.trim();
        let v = if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2) || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2) {
            v[1..v.len() - 1].to_string()
        } else {
            // strip an inline comment only when preceded by whitespace (`a=b #c`)
            match v.find(" #") {
                Some(i) => v[..i].trim_end().to_string(),
                None => v.to_string(),
            }
        };
        out.insert(k.to_string(), v);
    }
    out
}

fn is_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub fn interpolate(s: &str, vars: &Vars) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '$' {
            out.push(c);
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('$') => {
                out.push('$');
                i += 2;
            }
            Some('{') => {
                // find the matching brace (defaults may contain nested ${...})
                let mut depth = 0;
                let mut j = i + 1;
                while j < chars.len() {
                    match chars[j] {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if j >= chars.len() {
                    return Err(Error::Interp(format!("unclosed ${{ in {s:?}")));
                }
                let inner: String = chars[i + 2..j].iter().collect();
                out.push_str(&expand(&inner, vars, s)?);
                i = j + 1;
            }
            Some(&n) if is_name(n) && !n.is_ascii_digit() => {
                let mut j = i + 1;
                while j < chars.len() && is_name(chars[j]) {
                    j += 1;
                }
                let name: String = chars[i + 1..j].iter().collect();
                out.push_str(vars.get(&name).map(String::as_str).unwrap_or(""));
                i = j;
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
    Ok(out)
}

fn expand(inner: &str, vars: &Vars, whole: &str) -> Result<String> {
    let name_end = inner.find(|c: char| !is_name(c)).unwrap_or(inner.len());
    let (name, rest) = inner.split_at(name_end);
    if name.is_empty() {
        return Err(Error::Interp(format!("bad substitution ${{{inner}}} in {whole:?}")));
    }
    let val = vars.get(name);
    let set = val.is_some();
    let nonempty = val.is_some_and(|v| !v.is_empty());
    let sub = |t: &str| interpolate(t, vars);
    Ok(match rest {
        "" => val.cloned().unwrap_or_default(),
        r if r.starts_with(":-") => if nonempty { val.unwrap().clone() } else { sub(&r[2..])? },
        r if r.starts_with('-') => if set { val.unwrap().clone() } else { sub(&r[1..])? },
        r if r.starts_with(":?") => if nonempty { val.unwrap().clone() } else { return Err(Error::Interp(format!("{name}: {}", sub(&r[2..])?))) },
        r if r.starts_with('?') => if set { val.unwrap().clone() } else { return Err(Error::Interp(format!("{name}: {}", sub(&r[1..])?))) },
        r if r.starts_with(":+") => if nonempty { sub(&r[2..])? } else { String::new() },
        r if r.starts_with('+') => if set { sub(&r[1..])? } else { String::new() },
        _ => return Err(Error::Interp(format!("bad substitution ${{{inner}}} in {whole:?}"))),
    })
}

/// Interpolate every string inside a YAML value (keys too, as compose does for values only —
/// we leave keys alone).
pub fn interpolate_value(v: &mut serde_yaml::Value, vars: &Vars) -> Result<()> {
    match v {
        serde_yaml::Value::String(s) => *s = interpolate(s, vars)?,
        serde_yaml::Value::Sequence(seq) => {
            for x in seq {
                interpolate_value(x, vars)?;
            }
        }
        serde_yaml::Value::Mapping(m) => {
            for (_, x) in m.iter_mut() {
                interpolate_value(x, vars)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        [("A", "1"), ("EMPTY", ""), ("PATH_X", "/srv")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn forms() {
        let v = vars();
        assert_eq!(interpolate("$A ${A} x${A}y", &v).unwrap(), "1 1 x1y");
        assert_eq!(interpolate("${MISSING:-def} ${EMPTY:-def} ${EMPTY-def}", &v).unwrap(), "def def ");
        assert_eq!(interpolate("${A:+yes} ${EMPTY:+yes} ${EMPTY+yes}", &v).unwrap(), "yes  yes");
        assert_eq!(interpolate("$$HOME ${PATH_X}/data", &v).unwrap(), "$HOME /srv/data");
        assert_eq!(interpolate("${MISSING:-${A}}", &v).unwrap(), "1");
        assert!(interpolate("${MISSING:?must be set}", &v).unwrap_err().to_string().contains("must be set"));
        assert!(interpolate("${A", &v).is_err());
        assert_eq!(interpolate("cost: 5$", &v).unwrap(), "cost: 5$");
    }

    #[test]
    fn env_files() {
        let e = parse_env_file("# c\nA=1\nexport B=\"two words\"\nC='x'\nD=val # comment\nE=a#b\n=bad\n");
        assert_eq!(e["A"], "1");
        assert_eq!(e["B"], "two words");
        assert_eq!(e["C"], "x");
        assert_eq!(e["D"], "val");
        assert_eq!(e["E"], "a#b");
        assert!(!e.contains_key(""));
    }
}
