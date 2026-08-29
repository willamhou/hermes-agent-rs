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
UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/140 Safari/537.36"

s = requests.Session()
s.headers.update({"User-Agent": UA, "Accept-Language": "zh-CN,zh;q=0.9", "Referer": "https://weixin.sogou.com/"})
q = urllib.parse.quote(ACCOUNT)
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
    rr = s.get(row["href"], timeout=30, allow_redirects=True, headers={"Referer": search_url, "User-Agent": UA})
    (OUT / "resolved.html").write_bytes(rr.content)
    result.update({
        "status": rr.status_code,
        "final_url": rr.url,
        "history": [{"status": h.status_code, "url": h.url, "location": h.headers.get("location", "")} for h in rr.history],
        "bytes": len(rr.content),
        "content_type": rr.headers.get("content-type", ""),
    })
    text = rr.text
    result["bizs"] = sorted(set(re.findall(r"__biz=([A-Za-z0-9_=\-]+)", rr.url + "\n" + text)))
    result["mids"] = sorted(set(re.findall(r"(?:mid|appmsgid)[=:]['\"]?(\d+)", text)))
    result["nicknames"] = sorted(set(re.findall(r"(?:nickname|nick_name)\s*=\s*['\"]([^'\"]+)", text)))
    s2 = BeautifulSoup(text, "html.parser")
    metas = {}
    for meta in s2.find_all("meta"):
        key = meta.get("property") or meta.get("name")
        if key and meta.get("content"):
            metas[key] = meta.get("content")
    result["metas"] = metas
    result["page_title"] = s2.title.get_text(" ", strip=True) if s2.title else ""
    result["text_excerpt"] = " ".join(s2.get_text(" ", strip=True).split())[:5000]
except Exception as exc:
    result["error"] = repr(exc)

(OUT / "summary.json").write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
print(json.dumps(result, ensure_ascii=False, indent=2))
