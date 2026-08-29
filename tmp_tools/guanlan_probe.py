from __future__ import annotations

import asyncio
import html
import json
import re
import time
import urllib.parse
from pathlib import Path
from typing import Any

import requests
from bs4 import BeautifulSoup
from playwright.async_api import async_playwright

OUT = Path("guanlan_probe")
OUT.mkdir(exist_ok=True)

ACCOUNT = "观澜Horizon"
KNOWN_TITLE = "当AI吃掉全美8%的电力，谁在给芯片喂电"

UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
    "AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36"
)

session = requests.Session()
session.headers.update(
    {
        "User-Agent": UA,
        "Accept-Language": "zh-CN,zh;q=0.9,en;q=0.7",
        "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    }
)


def safe_name(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9\u4e00-\u9fff._-]+", "_", value).strip("_")[:120]


def parse_html(source_url: str, text: str) -> dict[str, Any]:
    soup = BeautifulSoup(text, "html.parser")
    links = []
    for a in soup.find_all("a", href=True):
        href = html.unescape(a.get("href", ""))
        label = " ".join(a.get_text(" ", strip=True).split())
        if any(
            marker in href
            for marker in [
                "mp.weixin.qq.com",
                "weixin.sogou.com/link",
                "/gzh?openid=",
                "/weixin?",
            ]
        ):
            links.append({"text": label[:500], "href": href})
    return {
        "source_url": source_url,
        "title": soup.title.get_text(" ", strip=True) if soup.title else "",
        "text_excerpt": " ".join(soup.get_text(" ", strip=True).split())[:12000],
        "openids": sorted(set(re.findall(r"openid=([A-Za-z0-9_-]+)", text))),
        "bizs": sorted(set(re.findall(r"__biz=([A-Za-z0-9_=\-]+)", text))),
        "links": links[:500],
    }


def request_probe() -> list[dict[str, Any]]:
    jobs: list[tuple[str, str]] = []
    queries = [ACCOUNT, f'"{ACCOUNT}"', KNOWN_TITLE, f'"{KNOWN_TITLE}"']
    for query in queries:
        encoded = urllib.parse.quote(query)
        jobs.extend(
            [
                (
                    f"https://weixin.sogou.com/weixin?type=1&query={encoded}&ie=utf8&s_from=input",
                    f"sogou_account_{safe_name(query)}.html",
                ),
                (
                    f"https://weixin.sogou.com/weixin?type=2&query={encoded}&ie=utf8&s_from=input",
                    f"sogou_article_{safe_name(query)}.html",
                ),
                (
                    f"https://www.baidu.com/s?wd={urllib.parse.quote('site:mp.weixin.qq.com/s ' + query)}",
                    f"baidu_{safe_name(query)}.html",
                ),
                (
                    f"https://www.bing.com/search?q={urllib.parse.quote('site:mp.weixin.qq.com/s ' + query)}",
                    f"bing_{safe_name(query)}.html",
                ),
                (
                    f"https://www.so.com/s?q={urllib.parse.quote('site:mp.weixin.qq.com/s ' + query)}",
                    f"so360_{safe_name(query)}.html",
                ),
            ]
        )

    results: list[dict[str, Any]] = []
    for url, filename in jobs:
        try:
            response = session.get(url, timeout=35, allow_redirects=True)
            (OUT / filename).write_bytes(response.content)
            item = parse_html(response.url, response.text)
            item.update(
                {
                    "requested_url": url,
                    "status": response.status_code,
                    "bytes": len(response.content),
                    "content_type": response.headers.get("content-type", ""),
                    "saved_as": filename,
                }
            )
            results.append(item)
            print(filename, response.status_code, len(response.content), response.url, flush=True)
        except Exception as exc:
            results.append({"requested_url": url, "error": repr(exc), "saved_as": filename})
            print("ERR", filename, repr(exc), flush=True)
        time.sleep(0.5)
    return results


async def browser_probe() -> list[dict[str, Any]]:
    jobs: list[tuple[str, str]] = []
    queries = [ACCOUNT, KNOWN_TITLE]
    for query in queries:
        encoded = urllib.parse.quote(query)
        jobs.extend(
            [
                (
                    f"https://weixin.sogou.com/weixin?type=1&query={encoded}&ie=utf8&s_from=input",
                    f"browser_sogou_account_{safe_name(query)}",
                ),
                (
                    f"https://weixin.sogou.com/weixin?type=2&query={encoded}&ie=utf8&s_from=input",
                    f"browser_sogou_article_{safe_name(query)}",
                ),
                (
                    f"https://www.baidu.com/s?wd={urllib.parse.quote('site:mp.weixin.qq.com/s ' + query)}",
                    f"browser_baidu_{safe_name(query)}",
                ),
                (
                    f"https://www.bing.com/search?q={urllib.parse.quote('site:mp.weixin.qq.com/s ' + query)}",
                    f"browser_bing_{safe_name(query)}",
                ),
            ]
        )

    results: list[dict[str, Any]] = []
    async with async_playwright() as p:
        browser = await p.chromium.launch(headless=True)
        context = await browser.new_context(
            user_agent=UA,
            locale="zh-CN",
            viewport={"width": 1440, "height": 1400},
        )
        page = await context.new_page()
        for url, stem in jobs:
            try:
                response = await page.goto(url, wait_until="domcontentloaded", timeout=60000)
                await page.wait_for_timeout(3500)
                text = await page.content()
                (OUT / f"{stem}.html").write_text(text, encoding="utf-8")
                await page.screenshot(path=str(OUT / f"{stem}.png"), full_page=True)
                item = parse_html(page.url, text)
                item.update(
                    {
                        "requested_url": url,
                        "status": response.status if response else None,
                        "saved_as": f"{stem}.html",
                        "screenshot": f"{stem}.png",
                    }
                )
                results.append(item)
                print(stem, response.status if response else None, page.url, flush=True)
            except Exception as exc:
                results.append({"requested_url": url, "error": repr(exc), "saved_as": stem})
                print("BROWSER ERR", stem, repr(exc), flush=True)
        await browser.close()
    return results


def main() -> None:
    request_results = request_probe()
    browser_results = asyncio.run(browser_probe())
    summary = {"account": ACCOUNT, "known_title": KNOWN_TITLE, "requests": request_results, "browser": browser_results}
    (OUT / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2)[:40000])


if __name__ == "__main__":
    main()
