from __future__ import annotations

import json
import re
import time
import urllib.parse
from datetime import datetime, timezone
from pathlib import Path

import requests
from bs4 import BeautifulSoup

OUT = Path("guanlan_pages_probe")
OUT.mkdir(exist_ok=True)
ACCOUNT = "观澜Horizon"
QUERY = urllib.parse.quote(ACCOUNT)
UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/140 Safari/537.36"

s = requests.Session()
s.headers.update({"User-Agent": UA, "Accept-Language": "zh-CN,zh;q=0.9", "Referer": "https://weixin.sogou.com/"})

pages = []
all_rows = []
for page in range(1, 21):
    url = f"https://weixin.sogou.com/weixin?query={QUERY}&s_from=input&type=2&page={page}&ie=utf8"
    try:
        r = s.get(url, timeout=18, allow_redirects=True)
        text = r.text
        (OUT / f"page_{page:02d}.html").write_bytes(r.content)
        soup = BeautifulSoup(text, "html.parser")
        rows = []
        for li in soup.select("ul.news-list li"):
            a = li.select_one("h3 a[href]")
            author_el = li.select_one("span.all-time-y2")
            script = li.select_one("span.s2 script")
            if not a:
                continue
            author = " ".join(author_el.get_text(" ", strip=True).split()) if author_el else ""
            ts_match = re.search(r"timeConvert\(['\"]?(\d+)", script.get_text(" ", strip=True) if script else "")
            ts = int(ts_match.group(1)) if ts_match else None
            row = {
                "page": page,
                "title": " ".join(a.get_text(" ", strip=True).split()),
                "author": author,
                "timestamp": ts,
                "date_utc": datetime.fromtimestamp(ts, tz=timezone.utc).isoformat() if ts else "",
                "sogou_href": urllib.parse.urljoin("https://weixin.sogou.com", a.get("href", "")),
                "d": li.get("d", ""),
            }
            rows.append(row)
            if author == ACCOUNT:
                all_rows.append(row)
        pages.append({
            "page": page,
            "status": r.status_code,
            "bytes": len(r.content),
            "title": soup.title.get_text(" ", strip=True) if soup.title else "",
            "result_count": len(rows),
            "target_count": sum(1 for x in rows if x["author"] == ACCOUNT),
            "has_captcha": any(k in text for k in ["验证码", "antispider", "请输入验证码"]),
            "excerpt": " ".join(soup.get_text(" ", strip=True).split())[:1500],
        })
        print(page, pages[-1], flush=True)
    except Exception as exc:
        pages.append({"page": page, "error": repr(exc)})
        print("ERR", page, repr(exc), flush=True)
    time.sleep(0.4)

unique = {}
for row in all_rows:
    unique[(row["title"], row["timestamp"])] = row
summary = {
    "account": ACCOUNT,
    "pages": pages,
    "target_rows_raw": len(all_rows),
    "target_rows_unique": len(unique),
    "min_timestamp": min((r["timestamp"] for r in unique.values() if r["timestamp"]), default=None),
    "max_timestamp": max((r["timestamp"] for r in unique.values() if r["timestamp"]), default=None),
    "rows": sorted(unique.values(), key=lambda r: r["timestamp"] or 0, reverse=True),
}
(OUT / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
print(json.dumps(summary, ensure_ascii=False, indent=2)[:80000])
