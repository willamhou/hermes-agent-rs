from __future__ import annotations

import html
import json
import re
import urllib.parse
from pathlib import Path

import requests
from bs4 import BeautifulSoup

OUT = Path("guanlan_fast_probe")
OUT.mkdir(exist_ok=True)

ACCOUNT = "观澜Horizon"
KNOWN_TITLE = "当AI吃掉全美8%的电力，谁在给芯片喂电"
UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/140 Safari/537.36"

s = requests.Session()
s.headers.update({"User-Agent": UA, "Accept-Language": "zh-CN,zh;q=0.9", "Referer": "https://weixin.sogou.com/"})

jobs = []
for q in [ACCOUNT, KNOWN_TITLE]:
    e = urllib.parse.quote(q)
    jobs += [
        (f"https://weixin.sogou.com/weixin?type=1&query={e}&ie=utf8&s_from=input", f"account_{len(jobs)}.html"),
        (f"https://weixin.sogou.com/weixin?type=2&query={e}&ie=utf8&s_from=input", f"article_{len(jobs)}.html"),
        (f"https://www.baidu.com/s?wd={urllib.parse.quote('site:mp.weixin.qq.com/s '+q)}", f"baidu_{len(jobs)}.html"),
    ]

summary = []
for url, fn in jobs:
    try:
        r = s.get(url, timeout=15, allow_redirects=True)
        (OUT / fn).write_bytes(r.content)
        text = r.text
        soup = BeautifulSoup(text, "html.parser")
        links = []
        for a in soup.find_all("a", href=True):
            href = html.unescape(a.get("href", ""))
            label = " ".join(a.get_text(" ", strip=True).split())
            if "weixin" in href or "mp.weixin.qq.com" in href:
                links.append({"text": label[:300], "href": href})
        summary.append({
            "requested": url,
            "status": r.status_code,
            "final_url": r.url,
            "bytes": len(r.content),
            "title": soup.title.get_text(" ", strip=True) if soup.title else "",
            "openids": sorted(set(re.findall(r"openid=([A-Za-z0-9_-]+)", text))),
            "bizs": sorted(set(re.findall(r"__biz=([A-Za-z0-9_=\-]+)", text))),
            "links": links[:300],
            "excerpt": " ".join(soup.get_text(" ", strip=True).split())[:8000],
            "saved_as": fn,
        })
    except Exception as exc:
        summary.append({"requested": url, "error": repr(exc), "saved_as": fn})

(OUT / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
print(json.dumps(summary, ensure_ascii=False, indent=2)[:50000])
