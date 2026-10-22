//! Watch checks without a browser or a model, when the page allows it: fetch it, read the
//! shop data most product pages carry (schema.org JSON-LD, Open Graph / microdata price and
//! availability), or compare a fingerprint of the visible text. Only when that can't tell,
//! the page text goes to the local model (one short completion), and only after that to the
//! browser agent. Most checks never touch the GPU.

use serde_json::{json, Value};
use sha2::Digest;

use crate::watches::{Check, Watch};
use crate::Lab;

/// The model server OG-Host's AI uses (set by the embedding server).
#[derive(Debug, Clone, Default)]
pub struct AiCfg {
    pub url: String,
    pub model: String,
    pub key: String,
}

pub struct Page {
    pub status: u16,
    pub html: String,
    pub text: String,
}

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

pub async fn fetch(lab: &Lab, url: &str) -> anyhow::Result<Page> {
    let r = lab
        .http
        .get(url)
        .header("user-agent", UA)
        .header("accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8")
        .header("accept-language", "en-US,en;q=0.8")
        .timeout(std::time::Duration::from_secs(25))
        .send()
        .await?;
    let status = r.status().as_u16();
    let bytes = r.bytes().await?;
    let html = String::from_utf8_lossy(&bytes[..bytes.len().min(4 << 20)]).into_owned();
    let text = visible_text(&html);
    Ok(Page { status, html, text })
}

/// Visible text: no scripts, styles or tags; whitespace collapsed.
pub fn visible_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 4);
    let mut i = 0;
    let b = html.as_bytes();
    while i < b.len() {
        if b[i] == b'<' {
            for tag in ["script", "style", "noscript", "svg", "template"] {
                if lower[i + 1..].starts_with(tag) {
                    if let Some(end) = lower[i..].find(&format!("</{tag}")) {
                        i += end;
                        break;
                    }
                }
            }
            match html[i..].find('>') {
                Some(e) => {
                    i += e + 1;
                    out.push(' ');
                }
                None => break,
            }
        } else {
            let next = html[i..].find('<').map(|n| i + n).unwrap_or(b.len());
            out.push_str(&html[i..next]);
            i = next;
        }
    }
    let decoded = out.replace("&nbsp;", " ").replace("&amp;", "&").replace("&#36;", "$").replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn blocked(p: &Page) -> bool {
    let t = p.text.to_ascii_lowercase();
    (p.status == 403 || p.status == 503 || p.status == 429) && (t.contains("just a moment") || t.contains("captcha") || t.contains("verify you are") || t.contains("access denied") || t.len() < 400)
        || t.contains("enable javascript and cookies to continue")
}

#[derive(Debug, Default, PartialEq)]
pub struct Shop {
    pub in_stock: Option<bool>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub name: Option<String>,
}

fn availability(s: &str) -> Option<bool> {
    let l = s.to_ascii_lowercase();
    if ["outofstock", "soldout", "discontinued", "out_of_stock", "out of stock", "oos"].iter().any(|k| l.contains(k)) {
        Some(false)
    } else if ["instock", "in_stock", "in stock", "limitedavailability", "onlineonly", "instoreonly", "preorder", "backorder"].iter().any(|k| l.contains(k)) {
        Some(true)
    } else {
        None
    }
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.replace(',', "").trim().parse().ok()))
}

/// schema.org Product data (JSON-LD), then meta / microdata tags.
pub fn shop_data(html: &str) -> Shop {
    let mut shop = Shop::default();
    let lower = html.to_ascii_lowercase();
    let mut at = 0;
    while let Some(start) = lower[at..].find("application/ld+json") {
        let s = at + start;
        let Some(open) = lower[s..].find('>') else { break };
        let body_start = s + open + 1;
        let Some(close) = lower[body_start..].find("</script") else { break };
        let body = &html[body_start..body_start + close];
        at = body_start + close;
        let Ok(v) = serde_json::from_str::<Value>(body.trim()) else { continue };
        let mut stack = vec![v];
        while let Some(x) = stack.pop() {
            match &x {
                Value::Array(a) => stack.extend(a.iter().cloned()),
                Value::Object(o) => {
                    let ty = o.get("@type").map(|t| t.to_string().to_ascii_lowercase()).unwrap_or_default();
                    if ty.contains("product") && shop.name.is_none() {
                        shop.name = o.get("name").and_then(|n| n.as_str()).map(String::from);
                    }
                    if ty.contains("offer") || o.contains_key("availability") || o.contains_key("price") {
                        if shop.in_stock.is_none() {
                            shop.in_stock = o.get("availability").and_then(|a| a.as_str().map(String::from).or_else(|| a.get("@id").and_then(|i| i.as_str()).map(String::from))).and_then(|a| availability(&a));
                        }
                        if shop.price.is_none() {
                            shop.price = o.get("price").and_then(num).or_else(|| o.get("lowPrice").and_then(num));
                            shop.currency = o.get("priceCurrency").and_then(|c| c.as_str()).map(String::from);
                        }
                    }
                    for (k, v) in o {
                        if k == "offers" || k == "@graph" || v.is_object() || v.is_array() {
                            stack.push(v.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // <meta property="product:price:amount" content="…"> / itemprop="price" / availability
    let attr = |tag: &str, key: &str| -> Option<String> {
        let i = lower.find(key)?;
        let tstart = lower[..i].rfind('<')?;
        let tend = tstart + lower[tstart..].find('>')?;
        let t = &html[tstart..tend];
        let tl = &lower[tstart..tend];
        let c = tl.find(&format!("{tag}=\""))?;
        let v = &t[c + tag.len() + 2..];
        Some(v[..v.find('"')?].to_string())
    };
    if shop.price.is_none() {
        shop.price = ["product:price:amount", "og:price:amount", "itemprop=\"price\""].iter().find_map(|k| attr("content", k)).and_then(|v| v.replace(',', "").parse().ok());
    }
    if shop.in_stock.is_none() {
        shop.in_stock = ["product:availability", "og:availability"].iter().find_map(|k| attr("content", k)).and_then(|v| availability(&v)).or_else(|| attr("href", "itemprop=\"availability\"").and_then(|v| availability(&v)));
    }
    shop
}

/// Wording on the page, only when it points one way.
fn stock_words(text: &str) -> Option<bool> {
    let l = text.to_ascii_lowercase();
    let out = ["out of stock", "sold out", "currently unavailable", "notify me when available", "temporarily out of stock", "no longer available"].iter().any(|k| l.contains(k));
    let inn = ["add to cart", "add to basket", "add to bag", "in stock", "buy now", "ships today"].iter().any(|k| l.contains(k));
    match (out, inn) {
        (true, false) => Some(false),
        (false, true) => Some(true),
        _ => None,
    }
}

pub fn fingerprint(text: &str) -> String {
    // digits that tick (counters, timestamps) shouldn't count as a change
    let norm: String = text.chars().filter(|c| !c.is_ascii_digit()).collect::<String>().to_lowercase();
    hex::encode(&sha2::Sha256::digest(norm.as_bytes())[..12])
}

/// The deterministic check. Some(result) = decided; None = ask something smarter.
pub fn decide(w: &Watch, page: &Page, prev: Option<&Check>) -> Option<Check> {
    let now = crate::now_ms();
    if blocked(page) {
        return None;
    }
    if page.status >= 400 {
        return Some(Check { at: now, status: "error".into(), summary: format!("the page answered HTTP {}", page.status), ..Default::default() });
    }
    let fp = fingerprint(&page.text);
    match w.kind.as_str() {
        "restock" | "price" => {
            let shop = shop_data(&page.html);
            let stock = shop.in_stock.or_else(|| stock_words(&page.text));
            if w.kind == "price" && shop.price.is_none() {
                return None;
            }
            let stock = stock?;
            let price_txt = shop.price.map(|p| format!(" at {}{p:.2}", if shop.currency.as_deref().unwrap_or("USD") == "USD" { "$" } else { "" })).unwrap_or_default();
            let summary = format!("{}{price_txt}", if stock { "In stock" } else { "Out of stock" });
            let changed = prev.is_some_and(|p| p.in_stock != Some(stock) || (shop.price.is_some() && p.price != shop.price));
            Some(Check { at: now, status: "ok".into(), in_stock: Some(stock), price: shop.price, summary, changed, fingerprint: fp })
        }
        _ => {
            // change: same text as last time = nothing to look at
            let same = prev.is_some_and(|p| !p.fingerprint.is_empty() && p.fingerprint == fp);
            if same {
                return Some(Check { at: now, status: "ok".into(), summary: prev.map(|p| p.summary.clone()).unwrap_or_default(), changed: false, fingerprint: fp, ..Default::default() });
            }
            None
        }
    }
}

/// One short completion from the local model over the page text.
pub async fn ask_model(lab: &Lab, w: &Watch, page: &Page, prev: Option<&Check>) -> anyhow::Result<Option<Check>> {
    let cfg = lab.ai.read().unwrap().clone();
    let Some(cfg) = cfg.filter(|c| !c.url.is_empty()) else { return Ok(None) };
    let text: String = page.text.chars().take(7000).collect();
    let prev_txt = prev.map(|p| format!("Previous result: {} (in_stock {:?}, price {:?})", p.summary, p.in_stock, p.price)).unwrap_or_else(|| "No previous result.".into());
    let body = json!({
        "model": if cfg.model.is_empty() { "local" } else { &cfg.model },
        "temperature": 0,
        "cache_prompt": true,
        "messages": [
            {"role": "system", "content": "You check a web page for a homelab owner. Reply with ONE JSON object and nothing else: {\"status\":\"ok\"|\"blocked\"|\"error\", \"in_stock\": true|false|null, \"price\": number|null, \"summary\": \"one short sentence for a phone\", \"changed\": true|false}. status blocked = the text is a robot check or login wall."},
            {"role": "user", "content": format!("Watch: {} ({} watch). Question: {}\n{prev_txt}\nPage {} text:\n{text}", w.name, w.kind, w.question, w.url)},
        ]
    });
    let mut rq = lab.http.post(format!("{}/chat/completions", cfg.url.trim_end_matches('/'))).json(&body).timeout(std::time::Duration::from_secs(120));
    if !cfg.key.is_empty() {
        rq = rq.bearer_auth(&cfg.key);
    }
    let v: Value = rq.send().await?.error_for_status()?.json().await?;
    let content = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
    let j = content.find('{').and_then(|s| content.rfind('}').map(|e| &content[s..=e])).and_then(|x| serde_json::from_str::<Value>(x).ok());
    let Some(j) = j else { return Ok(None) };
    let status = j["status"].as_str().unwrap_or("ok");
    if status == "blocked" {
        return Ok(None);
    }
    Ok(Some(Check {
        at: crate::now_ms(),
        status: if status == "error" { "error" } else { "ok" }.into(),
        in_stock: j["in_stock"].as_bool(),
        price: j["price"].as_f64(),
        summary: j["summary"].as_str().unwrap_or("").chars().take(200).collect(),
        changed: j["changed"].as_bool().unwrap_or(false),
        fingerprint: fingerprint(&page.text),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_shop_data_and_text() {
        let html = r#"<html><head><script type="application/ld+json">{"@context":"https://schema.org","@type":"Product","name":"RTX 5090","offers":{"@type":"Offer","price":"1999.99","priceCurrency":"USD","availability":"https://schema.org/OutOfStock"}}</script></head><body><h1>RTX 5090</h1><script>var x=1;</script><p>Sold out</p></body></html>"#;
        let s = shop_data(html);
        assert_eq!(s.in_stock, Some(false));
        assert_eq!(s.price, Some(1999.99));
        assert_eq!(s.name.as_deref(), Some("RTX 5090"));
        assert_eq!(visible_text(html), "RTX 5090 Sold out");
        let meta = r#"<meta property="product:price:amount" content="49.00"><link itemprop="availability" href="http://schema.org/InStock">"#;
        let m = shop_data(meta);
        assert_eq!((m.price, m.in_stock), (Some(49.0), Some(true)));
        assert_eq!(stock_words("Add to cart now"), Some(true));
        assert_eq!(stock_words("Sold out. Add to cart"), None);
        assert_eq!(fingerprint("Visitors: 123"), fingerprint("Visitors: 456"));
    }
}
