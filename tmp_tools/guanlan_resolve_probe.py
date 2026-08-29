from __future__ import annotations

import html
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
    result.update({"sogou_status": rr.status_code, "sogou_final_url": rr.url, "sogou_bytes": len(rr.content), "redirect_fragments": fragments, "mp_signature_url": mp_url})
    if not mp_url.startswith("https://mp.weixin.qq.com/"):
        raise RuntimeError(f"Failed to reconstruct mp URL: {mp_url!r}")

    mr = s.get(mp_url, timeout=45, allow_redirects=True, headers={"User-Agent": WECHAT_UA, "Accept-Language": "zh-CN,zh;q=0.9", "Referer": "https://weixin.sogou.com/"})
    (OUT / "mp_article.html").write_bytes(mr.content)
    text = mr.text
    soup2 = BeautifulSoup(text, "html.parser")
    metas = {}
    for meta in soup2.find_all("meta"):
        key = meta.get("property") or meta.get("name")
        if key and meta.get("content"):
            metas[key] = meta.get("content")

    def first(patterns):
        for pat in patterns:
            m = re.search(pat, text)
            if m and m.group(1):
                return html.unescape(m.group(1))
        return ""

    fields = {
        "biz": first([r"var\s+biz\s*=\s*['\"]([^'\"]+)", r"__biz=([A-Za-z0-9_=\-]+)"]),
        "mid": first([r"var\s+mid\s*=\s*['\"]?(\d+)", r"(?:mid|appmsgid)=([0-9]+)"]),
        "idx": first([r"var\s+idx\s*=\s*['\"]?(\d+)", r"idx=([0-9]+)"]),
        "ct": first([r"var\s+ct\s*=\s*['\"]?(\d+)"]),
        "nickname": first([r"var\s+nickname\s*=\s*htmlDecode\(['\"]([^'\"]+)", r"var\s+nickname\s*=\s*['\"]([^'\"]+)"]),
        "user_name": first([r"var\s+user_name\s*=\s*['\"]([^'\"]+)"]),
        "msg_title": first([r"var\s+msg_title\s*=\s*['\"]([^'\"]+)"]),
    }
    biz, mid, idx = fields["biz"], fields["mid"], fields["idx"]
    candidate_urls = [
        f"https://mp.weixin.qq.com/s?__biz={urllib.parse.quote(biz)}&mid={mid}&idx={idx}",
        f"https://mp.weixin.qq.com/s?__biz={urllib.parse.quote(biz)}&mid={mid}&idx={idx}&sn=",
        f"https://mp.weixin.qq.com/mp/appmsg/show?__biz={urllib.parse.quote(biz)}&appmsgid={mid}&itemidx={idx}",
    ]
    candidate_results = []
    for number, candidate in enumerate(candidate_urls):
        cr = s.get(candidate, timeout=45, allow_redirects=True, headers={"User-Agent": WECHAT_UA, "Referer": mp_url})
        (OUT / f"candidate_{number}.html").write_bytes(cr.content)
        cs = BeautifulSoup(cr.text, "html.parser")
        cvars = {
            "biz": bool(re.search(rf"var\s+biz\s*=\s*['\"]{re.escape(biz)}", cr.text)),
            "mid": bool(re.search(rf"var\s+mid\s*=\s*['\"]?{re.escape(mid)}", cr.text)),
            "idx": bool(re.search(rf"var\s+idx\s*=\s*['\"]?{re.escape(idx)}", cr.text)),
        }
        candidate_results.append({
            "url": candidate,
            "status": cr.status_code,
            "final_url": cr.url,
            "bytes": len(cr.content),
            "content_type": cr.headers.get("content-type", ""),
            "title": cs.title.get_text(" ", strip=True) if cs.title else "",
            "meta_title": (cs.find("meta", property="og:title") or {}).get("content", "") if cs.find("meta", property="og:title") else "",
            "article_vars_match": cvars,
            "environment_error": any(x in cr.text for x in ["环境异常", "访问过于频繁", "系统繁忙", "请在微信客户端打开", "参数错误"]),
            "excerpt": " ".join(cs.get_text(" ", strip=True).split())[:1000],
        })

    result.update({
        "mp_status": mr.status_code,
        "mp_final_url": mr.url,
        "mp_bytes": len(mr.content),
        "metas": metas,
        "fields": fields,
        "candidate_results": candidate_results,
        "text_excerpt": " ".join(soup2.get_text(" ", strip=True).split())[:3000],
    })
except Exception as exc:
    result["error"] = repr(exc)

(OUT / "summary.json").write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
print(json.dumps(result, ensure_ascii=False, indent=2))
