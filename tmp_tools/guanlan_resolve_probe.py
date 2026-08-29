from __future__ import annotations

import json
import re
import urllib.parse
from pathlib import Path

import requests
from bs4 import BeautifulSoup

OUT = Path("guanlan_resolve_probe")
OUT.mkdir(exist_ok=True)
ACCOUNT = "观澜Horizon"
DESKTOP_UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/140 Safari/537.36"
WECHAT_UA = "Mozilla/5.0 (Linux; Android 13; Pixel 7 Pro Build/TQ3A.230805.001; wv) AppleWebKit/537.36 Version/4.0 Chrome/112.0.0.0 Mobile Safari/537.36 MicroMessenger/8.0.47.2560(0x28002F37) WeChat/arm64 Weixin NetType/WIFI Language/zh_CN ABI/arm64"

s = requests.Session()
s.headers.update({"User-Agent": DESKTOP_UA, "Accept-Language": "zh-CN,zh;q=0.9", "Referer": "https://weixin.sogou.com/"})
q = urllib.parse.quote(f'"{ACCOUNT}"')
search_url = f"https://weixin.sogou.com/weixin?query={q}&s_from=input&type=2&page=1&ie=utf8"
r = s.get(search_url, timeout=20)
(OUT / "search.html").write_bytes(r.content)
soup = BeautifulSoup(r.text, "html.parser")
row = None
for li in soup.select("ul.news-list li"):
    author = li.select_one("span.all-time-y2")
    a = li.select_one("h3 a[href]")
    if author and a and " ".join(author.get_text(" ", strip=True).split()) == ACCOUNT:
        row = {"title": " ".join(a.get_text(" ", strip=True).split()), "href": urllib.parse.urljoin("https://weixin.sogou.com", a.get("href"))}
        break
if not row:
    raise SystemExit("No target result")

result = {"search_url": search_url, **row}
try:
    rr = s.get(row["href"], timeout=30, allow_redirects=True, headers={"Referer": search_url, "User-Agent": DESKTOP_UA})
    (OUT / "resolved.html").write_bytes(rr.content)
    redirect_text = rr.content.decode("gbk", errors="ignore")
    fragments = re.findall(r"url\s*\+=\s*['\"]([^'\"]*)['\"]\s*;", redirect_text)
    mp_url = "".join(fragments).replace("@", "")
    result.update({
        "sogou_status": rr.status_code,
        "sogou_final_url": rr.url,
        "sogou_bytes": len(rr.content),
        "redirect_fragments": fragments,
        "mp_signature_url": mp_url,
    })
    if not mp_url.startswith("https://mp.weixin.qq.com/"):
        raise RuntimeError(f"Failed to reconstruct mp URL: {mp_url!r}")

    mr = s.get(
        mp_url,
        timeout=45,
        allow_redirects=True,
        headers={
            "User-Agent": WECHAT_UA,
            "Accept-Language": "zh-CN,zh;q=0.9",
            "Referer": "https://weixin.sogou.com/",
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        },
    )
    (OUT / "mp_article.html").write_bytes(mr.content)
    text = mr.text
    soup2 = BeautifulSoup(text, "html.parser")
    metas = {}
    for meta in soup2.find_all("meta"):
        key = meta.get("property") or meta.get("name")
        if key and meta.get("content"):
            metas[key] = meta.get("content")

    patterns = {
        "biz": [r"var\s+biz\s*=\s*['\"]([^'\"]+)", r"__biz=([A-Za-z0-9_=\-]+)"],
        "mid": [r"var\s+mid\s*=\s*['\"]?(\d+)", r"(?:mid|appmsgid)=([0-9]+)"],
        "idx": [r"var\s+idx\s*=\s*['\"]?(\d+)", r"idx=([0-9]+)"],
        "sn": [r"var\s+sn\s*=\s*['\"]([^'\"]+)", r"sn=([A-Za-z0-9]+)"],
        "nickname": [r"var\s+nickname\s*=\s*['\"]([^'\"]+)", r"var\s+nick_name\s*=\s*['\"]([^'\"]+)"],
        "msg_title": [r"var\s+msg_title\s*=\s*['\"]([^'\"]+)"],
        "ct": [r"var\s+ct\s*=\s*['\"]?(\d+)"],
    }
    fields = {}
    for key, regexes in patterns.items():
        vals = []
        for regex in regexes:
            vals.extend(re.findall(regex, text))
        fields[key] = list(dict.fromkeys(vals))

    biz = fields["biz"][0] if fields["biz"] else ""
    mid = fields["mid"][0] if fields["mid"] else ""
    idx = fields["idx"][0] if fields["idx"] else ""
    sn = fields["sn"][0] if fields["sn"] else ""
    canonical = f"https://mp.weixin.qq.com/s?__biz={urllib.parse.quote(biz)}&mid={mid}&idx={idx}&sn={sn}" if all([biz, mid, idx, sn]) else metas.get("og:url", "")

    result.update({
        "mp_status": mr.status_code,
        "mp_final_url": mr.url,
        "mp_bytes": len(mr.content),
        "mp_content_type": mr.headers.get("content-type", ""),
        "mp_title": soup2.title.get_text(" ", strip=True) if soup2.title else "",
        "metas": metas,
        "fields": fields,
        "canonical_url": canonical,
        "text_excerpt": " ".join(soup2.get_text(" ", strip=True).split())[:6000],
        "contains_environment_error": any(x in text for x in ["环境异常", "访问过于频繁", "系统繁忙", "请在微信客户端打开"]),
    })
except Exception as exc:
    result["error"] = repr(exc)

(OUT / "summary.json").write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
print(json.dumps(result, ensure_ascii=False, indent=2))
