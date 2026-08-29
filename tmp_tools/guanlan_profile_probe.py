from __future__ import annotations

import asyncio
import html
import json
import re
import urllib.parse
from pathlib import Path
from typing import Any

import requests
from bs4 import BeautifulSoup
from playwright.async_api import async_playwright

OUT = Path("guanlan_profile_probe")
OUT.mkdir(exist_ok=True)
ACCOUNT = "观澜Horizon"
DESKTOP_UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/140 Safari/537.36"
WECHAT_UA = "Mozilla/5.0 (Linux; Android 13; Pixel 7 Pro Build/TQ3A.230805.001; wv) AppleWebKit/537.36 Version/4.0 Chrome/112.0.0.0 Mobile Safari/537.36 MicroMessenger/8.0.47.2560(0x28002F37) WeChat/arm64 Weixin NetType/WIFI Language/zh_CN ABI/arm64"


def extract_js_concat(text: str) -> str:
    return "".join(re.findall(r"url\s*\+=\s*['\"]([^'\"]*)['\"]\s*;", text)).replace("@", "")


def extract_vars(text: str) -> dict[str, str]:
    patterns = {
        "biz": [r"var\s+biz\s*=\s*['\"]([^'\"]+)", r"__biz=([A-Za-z0-9_=\-]+)"],
        "mid": [r"var\s+mid\s*=\s*['\"]?(\d+)", r"(?:mid|appmsgid)=([0-9]+)"],
        "idx": [r"var\s+idx\s*=\s*['\"]?(\d+)", r"idx=([0-9]+)"],
        "ct": [r"var\s+ct\s*=\s*['\"]?(\d+)"],
        "nickname": [r"var\s+nickname\s*=\s*htmlDecode\(['\"]([^'\"]+)", r"var\s+nickname\s*=\s*['\"]([^'\"]+)"],
        "user_name": [r"var\s+user_name\s*=\s*['\"]([^'\"]+)"],
        "msg_title": [r"var\s+msg_title\s*=\s*['\"]([^'\"]+)"],
    }
    out: dict[str, str] = {}
    for key, pats in patterns.items():
        value = ""
        for pat in pats:
            match = re.search(pat, text)
            if match and match.group(1):
                value = html.unescape(match.group(1))
                break
        out[key] = value
    return out


def find_json_candidates(text: str) -> list[dict[str, Any]]:
    candidates: list[dict[str, Any]] = []
    markers = ["msgList", "msg_list", "appmsg_list", "publish_page", "list"]
    for marker in markers:
        for match in re.finditer(marker, text, re.I):
            start = max(0, match.start() - 100)
            snippet = text[start:match.start() + 200000]
            for open_char, close_char in [("{", "}"), ("[", "]")]:
                pos = snippet.find(open_char)
                if pos < 0:
                    continue
                depth = 0
                in_str = False
                escaped = False
                quote = ""
                for i, ch in enumerate(snippet[pos:], pos):
                    if in_str:
                        if escaped:
                            escaped = False
                        elif ch == "\\":
                            escaped = True
                        elif ch == quote:
                            in_str = False
                    else:
                        if ch in ["'", '"']:
                            in_str = True
                            quote = ch
                        elif ch == open_char:
                            depth += 1
                        elif ch == close_char:
                            depth -= 1
                            if depth == 0:
                                raw = snippet[pos:i + 1]
                                try:
                                    parsed = json.loads(raw)
                                    candidates.append({"marker": marker, "parsed": parsed, "raw_prefix": raw[:300]})
                                except Exception:
                                    pass
                                break
    return candidates[:50]


def request_stage() -> dict[str, Any]:
    s = requests.Session()
    s.headers.update({"User-Agent": DESKTOP_UA, "Accept-Language": "zh-CN,zh;q=0.9", "Referer": "https://weixin.sogou.com/"})
    query = urllib.parse.quote(f'"{ACCOUNT}"')
    search_url = f"https://weixin.sogou.com/weixin?query={query}&s_from=input&type=2&page=1&ie=utf8"
    sr = s.get(search_url, timeout=25)
    (OUT / "search.html").write_bytes(sr.content)
    soup = BeautifulSoup(sr.text, "html.parser")
    href = ""
    title = ""
    for li in soup.select("ul.news-list li"):
        author = li.select_one("span.all-time-y2")
        a = li.select_one("h3 a[href]")
        if author and a and " ".join(author.get_text(" ", strip=True).split()) == ACCOUNT:
            href = urllib.parse.urljoin("https://weixin.sogou.com", a.get("href", ""))
            title = " ".join(a.get_text(" ", strip=True).split())
            break
    if not href:
        raise RuntimeError("No target result")
    rr = s.get(href, timeout=30, headers={"Referer": search_url, "User-Agent": DESKTOP_UA})
    redirect_text = rr.content.decode("gbk", errors="ignore")
    (OUT / "redirect.html").write_bytes(rr.content)
    mp_url = extract_js_concat(redirect_text)
    if not mp_url:
        raise RuntimeError("No mp URL in Sogou redirect")
    ar = s.get(mp_url, timeout=50, headers={"User-Agent": WECHAT_UA, "Referer": "https://weixin.sogou.com/"})
    (OUT / "article.html").write_bytes(ar.content)
    vars_ = extract_vars(ar.text)
    biz = vars_.get("biz", "")
    if not biz:
        raise RuntimeError("No biz in article")

    profile_urls = [
        f"https://mp.weixin.qq.com/mp/profile_ext?action=home&__biz={urllib.parse.quote(biz)}&scene={scene}#wechat_redirect"
        for scene in [124, 123, 110]
    ]
    profile_results = []
    for i, url in enumerate(profile_urls):
        pr = s.get(url, timeout=50, headers={"User-Agent": WECHAT_UA, "Referer": mp_url})
        (OUT / f"profile_requests_{i}.html").write_bytes(pr.content)
        profile_results.append({
            "url": url,
            "status": pr.status_code,
            "final_url": pr.url,
            "bytes": len(pr.content),
            "content_type": pr.headers.get("content-type", ""),
            "excerpt": " ".join(BeautifulSoup(pr.text, "html.parser").get_text(" ", strip=True).split())[:3000],
            "json_candidates": find_json_candidates(pr.text),
        })

    getmsg_results = []
    for offset in [0, 10, 20, 30, 40, 50]:
        url = (
            "https://mp.weixin.qq.com/mp/profile_ext?action=getmsg"
            f"&__biz={urllib.parse.quote(biz)}&f=json&offset={offset}&count=10&is_ok=1&scene=124"
            "&uin=&key=&pass_ticket=&wxtoken=&x5=0"
        )
        gr = s.get(url, timeout=35, headers={"User-Agent": WECHAT_UA, "Referer": profile_urls[0], "X-Requested-With": "XMLHttpRequest"})
        (OUT / f"getmsg_requests_{offset}.txt").write_bytes(gr.content)
        try:
            body: Any = gr.json()
        except Exception:
            body = gr.text[:5000]
        getmsg_results.append({"offset": offset, "url": url, "status": gr.status_code, "bytes": len(gr.content), "body": body})
    return {
        "search_title": title,
        "search_url": search_url,
        "sogou_href": href,
        "mp_url": mp_url,
        "article_status": ar.status_code,
        "article_final_url": ar.url,
        "article_vars": vars_,
        "profile_results": profile_results,
        "getmsg_results": getmsg_results,
    }


async def browser_stage(seed: dict[str, Any]) -> dict[str, Any]:
    network: list[dict[str, Any]] = []
    async with async_playwright() as p:
        browser = await p.chromium.launch(headless=True)
        context = await browser.new_context(
            user_agent=WECHAT_UA,
            locale="zh-CN",
            viewport={"width": 430, "height": 932},
            is_mobile=True,
            has_touch=True,
            device_scale_factor=2,
        )
        page = await context.new_page()

        async def on_response(response):
            url = response.url
            if any(x in url for x in ["profile_ext", "getmsg", "appmsg", "mp.weixin.qq.com/s"]):
                item: dict[str, Any] = {"url": url, "status": response.status, "content_type": response.headers.get("content-type", "")}
                try:
                    body = await response.body()
                    item["bytes"] = len(body)
                    filename = f"network_{len(network):03d}.bin"
                    (OUT / filename).write_bytes(body)
                    item["saved_as"] = filename
                    if len(body) < 2000000:
                        try:
                            item["text_excerpt"] = body.decode("utf-8", errors="ignore")[:5000]
                        except Exception:
                            pass
                except Exception as exc:
                    item["body_error"] = repr(exc)
                network.append(item)

        page.on("response", on_response)
        result: dict[str, Any] = {}
        try:
            resp = await page.goto(seed["mp_url"], wait_until="domcontentloaded", timeout=90000)
            await page.wait_for_timeout(5000)
            (OUT / "browser_article.html").write_text(await page.content(), encoding="utf-8")
            await page.screenshot(path=str(OUT / "browser_article.png"), full_page=True)
            result["article"] = {"status": resp.status if resp else None, "url": page.url, "title": await page.title()}
            try:
                name = page.locator("#js_name")
                if await name.count():
                    await name.first.click(timeout=10000)
                    await page.wait_for_timeout(6000)
            except Exception as exc:
                result["click_error"] = repr(exc)
            result["after_click_url"] = page.url
            (OUT / "browser_after_click.html").write_text(await page.content(), encoding="utf-8")
            await page.screenshot(path=str(OUT / "browser_after_click.png"), full_page=True)
        except Exception as exc:
            result["article_error"] = repr(exc)

        biz = seed.get("article_vars", {}).get("biz", "")
        profile_url = f"https://mp.weixin.qq.com/mp/profile_ext?action=home&__biz={urllib.parse.quote(biz)}&scene=124#wechat_redirect"
        try:
            resp2 = await page.goto(profile_url, wait_until="domcontentloaded", timeout=90000)
            await page.wait_for_timeout(8000)
            for _ in range(5):
                await page.evaluate("window.scrollTo(0, document.body.scrollHeight)")
                await page.wait_for_timeout(2500)
            profile_html = await page.content()
            (OUT / "browser_profile.html").write_text(profile_html, encoding="utf-8")
            await page.screenshot(path=str(OUT / "browser_profile.png"), full_page=True)
            result["profile"] = {
                "status": resp2.status if resp2 else None,
                "url": page.url,
                "title": await page.title(),
                "excerpt": " ".join(BeautifulSoup(profile_html, "html.parser").get_text(" ", strip=True).split())[:5000],
                "json_candidates": find_json_candidates(profile_html),
            }
        except Exception as exc:
            result["profile_error"] = repr(exc)
        result["network"] = network
        await browser.close()
        return result


def main() -> None:
    request_result = request_stage()
    browser_result = asyncio.run(browser_stage(request_result))
    summary = {"request": request_result, "browser": browser_result}
    (OUT / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2)[:120000])


if __name__ == "__main__":
    main()
