from __future__ import annotations

import csv
import html
import json
import random
import re
import time
import urllib.parse
from collections import defaultdict
from datetime import datetime, timezone, timedelta
from pathlib import Path
from typing import Any

import requests
from bs4 import BeautifulSoup

OUT = Path("guanlan_export")
OUT.mkdir(exist_ok=True)
ACCOUNT = "观澜Horizon"
EXPECTED_BIZ = "Mzk5MDA5NjY1OA=="
EXPECTED_USER_NAME = "gh_530fbfb1dbd0"
DESKTOP_UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/140 Safari/537.36"
WECHAT_UA = "Mozilla/5.0 (Linux; Android 13; Pixel 7 Pro Build/TQ3A.230805.001; wv) AppleWebKit/537.36 Version/4.0 Chrome/112.0.0.0 Mobile Safari/537.36 MicroMessenger/8.0.47.2560(0x28002F37) WeChat/arm64 Weixin NetType/WIFI Language/zh_CN ABI/arm64"
SHANGHAI = timezone(timedelta(hours=8))

KNOWN_EXTERNAL = [
    {
        "title": "当AI吃掉全美8%的电力，谁在给芯片喂电",
        "date": "2026-05-23",
        "source_url": "https://post.smzdm.com/p/arzw6pgx/",
        "source_note": "第三方文章明确引用观澜Horizon及该标题；原文未被当前搜狗精确标题检索返回。",
    }
]

TOPICS = [
    "2026", "2025", "AI", "人工智能", "算力", "数据中心", "电力", "电网", "储能", "逆变器",
    "能源", "新能源", "光伏", "风电", "核电", "煤炭", "甲醇", "芯片", "半导体", "存储", "HBM",
    "DRAM", "NAND", "国产替代", "供应链", "机器人", "智能", "创新药", "医药", "生物", "减肥药",
    "mRNA", "基因", "疫苗", "消费", "科技", "产业", "行业", "市场", "投资", "A股", "美股", "港股",
    "股票", "牛市", "周期", "业绩", "财报", "中报", "涨价", "价格", "公司", "中国", "美国", "全球",
    "为什么", "如何", "谁", "机会", "风险", "龙头", "暴涨", "回调", "上游", "下游", "利润", "订单",
    "的", "是", "在", "这", "不是", "一个",
    "谁在给芯片喂电", "AI吃掉全美", "全美8% 电力", "电力 芯片",
]

QUERY_SPECS: list[tuple[str, int]] = [
    (f'"{ACCOUNT}"', 10),
    (ACCOUNT, 10),
    ("观澜", 10),
    ("Horizon", 10),
]
QUERY_SPECS.extend((f"{ACCOUNT} {topic}", 3) for topic in TOPICS)
QUERY_SPECS.extend((f'"{ACCOUNT}" {topic}', 2) for topic in TOPICS[:55])


def clean(value: Any) -> str:
    text = html.unescape(str(value or ""))
    text = text.replace("\u200b", "").replace("\u200c", "").replace("\ufeff", "")
    return re.sub(r"\s+", " ", text).strip()


def normalize_title(value: str) -> str:
    text = clean(value).lower()
    text = text.replace("：", ":").replace("，", ",").replace("？", "?").replace("！", "!")
    return re.sub(r"[\s\u200b]+", "", text)


def new_session() -> requests.Session:
    session = requests.Session()
    session.headers.update(
        {
            "User-Agent": DESKTOP_UA,
            "Accept-Language": "zh-CN,zh;q=0.9,en;q=0.6",
            "Referer": "https://weixin.sogou.com/",
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        }
    )
    return session


def search_page(session: requests.Session, query: str, page: int) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    encoded = urllib.parse.quote(query)
    url = f"https://weixin.sogou.com/weixin?query={encoded}&s_from=input&type=2&page={page}&ie=utf8"
    response = session.get(url, timeout=25, allow_redirects=True)
    text = response.text
    soup = BeautifulSoup(text, "html.parser")
    rows: list[dict[str, Any]] = []
    for li in soup.select("ul.news-list li"):
        a = li.select_one("h3 a[href]")
        author_el = li.select_one("span.all-time-y2")
        script = li.select_one("span.s2 script")
        if not a:
            continue
        author = clean(author_el.get_text(" ", strip=True) if author_el else "")
        timestamp_match = re.search(r"timeConvert\(['\"]?(\d+)", script.get_text(" ", strip=True) if script else "")
        timestamp = int(timestamp_match.group(1)) if timestamp_match else None
        snippet_el = li.select_one("p.txt-info")
        rows.append(
            {
                "query": query,
                "search_page": page,
                "search_url": url,
                "title_sogou": clean(a.get_text(" ", strip=True)),
                "author_sogou": author,
                "timestamp_sogou": timestamp,
                "date_sogou": datetime.fromtimestamp(timestamp, tz=SHANGHAI).strftime("%Y-%m-%d %H:%M:%S") if timestamp else "",
                "sogou_href": urllib.parse.urljoin("https://weixin.sogou.com", a.get("href", "")),
                "sogou_doc_key": clean(li.get("d", "")),
                "snippet_sogou": clean(snippet_el.get_text(" ", strip=True) if snippet_el else ""),
            }
        )
    log = {
        "query": query,
        "page": page,
        "status": response.status_code,
        "bytes": len(response.content),
        "result_count": len(rows),
        "target_count": sum(1 for row in rows if row["author_sogou"] == ACCOUNT),
        "captcha": any(marker in text for marker in ["antispider", "请输入验证码", "验证码", "访问过于频繁"]),
        "title": soup.title.get_text(" ", strip=True) if soup.title else "",
        "url": response.url,
    }
    return log, rows


def first_match(text: str, patterns: list[str]) -> str:
    for pattern in patterns:
        match = re.search(pattern, text)
        if match and match.group(1):
            return clean(match.group(1))
    return ""


def parse_article(text: str) -> dict[str, Any]:
    soup = BeautifulSoup(text, "html.parser")
    metas: dict[str, str] = {}
    for meta in soup.find_all("meta"):
        key = meta.get("property") or meta.get("name")
        if key and meta.get("content"):
            metas[key] = clean(meta.get("content"))
    fields = {
        "biz": first_match(text, [r"var\s+biz\s*=\s*['\"]([^'\"]+)", r"__biz=([A-Za-z0-9_=\-]+)"]),
        "mid": first_match(text, [r"var\s+mid\s*=\s*['\"]?(\d+)", r"(?:mid|appmsgid)=([0-9]+)"]),
        "idx": first_match(text, [r"var\s+idx\s*=\s*['\"]?(\d+)", r"idx=([0-9]+)"]),
        "ct": first_match(text, [r"var\s+ct\s*=\s*['\"]?(\d+)"]),
        "nickname": first_match(text, [r"var\s+nickname\s*=\s*htmlDecode\(['\"]([^'\"]+)", r"var\s+nickname\s*=\s*['\"]([^'\"]+)"]),
        "user_name": first_match(text, [r"var\s+user_name\s*=\s*['\"]([^'\"]+)"]),
        "msg_title": first_match(text, [r"var\s+msg_title\s*=\s*['\"]([^'\"]+)"]),
        "author": first_match(text, [r"var\s+author\s*=\s*['\"]([^'\"]+)"]),
        "profile_signature": first_match(text, [r"var\s+profile_signature\s*=\s*['\"]([^'\"]*)"]),
    }
    if not fields["msg_title"]:
        fields["msg_title"] = metas.get("og:title", "") or metas.get("description", "")
    if not fields["nickname"]:
        fields["nickname"] = clean(soup.select_one("#js_name").get_text(" ", strip=True) if soup.select_one("#js_name") else "")
    timestamp = int(fields["ct"]) if fields["ct"].isdigit() else None
    durable_url = ""
    if fields["biz"] and fields["mid"] and fields["idx"]:
        durable_url = "https://mp.weixin.qq.com/s?" + urllib.parse.urlencode(
            {"__biz": fields["biz"], "mid": fields["mid"], "idx": fields["idx"]}
        )
    return {
        **fields,
        "title_wechat": fields["msg_title"],
        "description_wechat": metas.get("og:description", "") or metas.get("description", ""),
        "meta_author": metas.get("author", "") or metas.get("og:article:author", ""),
        "publish_timestamp": timestamp,
        "publish_datetime": datetime.fromtimestamp(timestamp, tz=SHANGHAI).strftime("%Y-%m-%d %H:%M:%S") if timestamp else "",
        "publish_date": datetime.fromtimestamp(timestamp, tz=SHANGHAI).strftime("%Y-%m-%d") if timestamp else "",
        "durable_identifier_url": durable_url,
        "text_excerpt": clean(soup.get_text(" ", strip=True))[:1000],
        "environment_error": any(marker in text for marker in ["环境异常", "访问过于频繁", "系统繁忙", "请在微信客户端打开"]),
    }


def resolve_row(session: requests.Session, row: dict[str, Any]) -> dict[str, Any]:
    result = dict(row)
    try:
        redirect = session.get(
            row["sogou_href"],
            timeout=30,
            allow_redirects=True,
            headers={"User-Agent": DESKTOP_UA, "Referer": row["search_url"]},
        )
        redirect_text = redirect.content.decode("gbk", errors="ignore")
        fragments = re.findall(r"url\s*\+=\s*['\"]([^'\"]*)['\"]\s*;", redirect_text)
        signature_url = "".join(fragments).replace("@", "")
        result.update(
            {
                "sogou_redirect_status": redirect.status_code,
                "sogou_redirect_bytes": len(redirect.content),
                "wechat_signature_url": signature_url,
            }
        )
        if not signature_url.startswith("https://mp.weixin.qq.com/"):
            raise RuntimeError("未能从搜狗跳转页还原微信链接")
        article_response = session.get(
            signature_url,
            timeout=55,
            allow_redirects=True,
            headers={
                "User-Agent": WECHAT_UA,
                "Accept-Language": "zh-CN,zh;q=0.9",
                "Referer": "https://weixin.sogou.com/",
            },
        )
        article = parse_article(article_response.text)
        result.update(article)
        result.update(
            {
                "wechat_status": article_response.status_code,
                "wechat_final_url_at_fetch": article_response.url,
                "wechat_bytes": len(article_response.content),
                "verified_account": article.get("biz") == EXPECTED_BIZ and article.get("user_name") == EXPECTED_USER_NAME and article.get("nickname") == ACCOUNT,
                "resolution_status": "已打开微信原文并核验账号" if article.get("biz") == EXPECTED_BIZ and article.get("nickname") == ACCOUNT else "微信页面已打开但账号字段未完全匹配",
            }
        )
    except Exception as exc:
        result.update({"resolution_status": "原文解析失败", "resolution_error": repr(exc), "verified_account": False})
    return result


def main() -> None:
    search_logs: list[dict[str, Any]] = []
    records_by_key: dict[str, dict[str, Any]] = {}
    queries_by_key: dict[str, set[str]] = defaultdict(set)
    session = new_session()
    session_query_count = 0

    for query_index, (query, max_pages) in enumerate(QUERY_SPECS, 1):
        if session_query_count >= 12:
            session.close()
            session = new_session()
            session_query_count = 0
        session_query_count += 1
        zero_target_pages = 0
        for page in range(1, max_pages + 1):
            try:
                log, rows = search_page(session, query, page)
            except Exception as exc:
                search_logs.append({"query": query, "page": page, "error": repr(exc)})
                session.close()
                session = new_session()
                break
            search_logs.append(log)
            print(f"[{query_index}/{len(QUERY_SPECS)}] {query!r} p{page}: results={log['result_count']} target={log['target_count']} captcha={log['captcha']}", flush=True)
            if log["captcha"]:
                session.close()
                session = new_session()
                time.sleep(2)
                break
            target_rows = [row for row in rows if row["author_sogou"] == ACCOUNT]
            if not target_rows:
                zero_target_pages += 1
            else:
                zero_target_pages = 0
            for row in target_rows:
                key = row["sogou_doc_key"] or f"{normalize_title(row['title_sogou'])}|{row.get('timestamp_sogou')}"
                queries_by_key[key].add(query)
                if key in records_by_key:
                    continue
                resolved = resolve_row(session, row)
                records_by_key[key] = resolved
                print("  NEW", resolved.get("publish_date") or row.get("date_sogou"), resolved.get("title_wechat") or row["title_sogou"], resolved.get("resolution_status"), flush=True)
                time.sleep(random.uniform(0.15, 0.35))
            if log["result_count"] == 0:
                break
            if page >= 2 and zero_target_pages >= 1:
                break
            time.sleep(random.uniform(0.18, 0.40))

    # Attach all discovery queries and deduplicate verified records by stable article identity.
    raw_records = []
    for key, record in records_by_key.items():
        record["discovery_queries"] = "；".join(sorted(queries_by_key[key]))
        raw_records.append(record)

    verified_map: dict[str, dict[str, Any]] = {}
    unresolved = []
    for record in raw_records:
        if record.get("verified_account") and record.get("biz") and record.get("mid") and record.get("idx"):
            stable_key = f"{record['biz']}|{record['mid']}|{record['idx']}"
            existing = verified_map.get(stable_key)
            if existing:
                existing["discovery_queries"] = "；".join(sorted(set((existing.get("discovery_queries", "") + "；" + record.get("discovery_queries", "")).split("；")) - {""}))
            else:
                verified_map[stable_key] = record
        else:
            unresolved.append(record)

    verified = sorted(
        verified_map.values(),
        key=lambda row: (int(row.get("publish_timestamp") or row.get("timestamp_sogou") or 0), row.get("title_wechat") or row.get("title_sogou") or ""),
        reverse=True,
    )

    verified_titles = {normalize_title(row.get("title_wechat") or row.get("title_sogou") or "") for row in verified}
    external_unresolved = []
    for item in KNOWN_EXTERNAL:
        if normalize_title(item["title"]) not in verified_titles:
            external_unresolved.append(
                {
                    "title_sogou": item["title"],
                    "publish_date": item["date"],
                    "author_sogou": ACCOUNT,
                    "resolution_status": "第三方引用确认账号与标题，未解析到原始微信链接",
                    "external_source_url": item["source_url"],
                    "external_source_note": item["source_note"],
                    "verified_account": False,
                }
            )
    unresolved.extend(external_unresolved)

    verified_headers = [
        "序号", "文章标题", "发布日期", "发布时间", "公众号名称", "作者署名",
        "原文永久标识链接", "抓取时微信直达链接", "搜狗索引链接",
        "__biz", "mid", "idx", "微信内部账号", "搜狗文档键",
        "搜狗标题", "搜狗发布日期", "文章摘要", "发现查询词", "核验状态", "抓取说明",
    ]
    verified_rows = []
    for number, record in enumerate(verified, 1):
        verified_rows.append(
            {
                "序号": number,
                "文章标题": record.get("title_wechat") or record.get("title_sogou", ""),
                "发布日期": record.get("publish_date") or (record.get("date_sogou", "")[:10]),
                "发布时间": record.get("publish_datetime") or record.get("date_sogou", ""),
                "公众号名称": record.get("nickname") or record.get("author_sogou", ""),
                "作者署名": record.get("author") or record.get("meta_author", ""),
                "原文永久标识链接": record.get("durable_identifier_url", ""),
                "抓取时微信直达链接": record.get("wechat_signature_url", ""),
                "搜狗索引链接": record.get("sogou_href", ""),
                "__biz": record.get("biz", ""),
                "mid": record.get("mid", ""),
                "idx": record.get("idx", ""),
                "微信内部账号": record.get("user_name", ""),
                "搜狗文档键": record.get("sogou_doc_key", ""),
                "搜狗标题": record.get("title_sogou", ""),
                "搜狗发布日期": record.get("date_sogou", ""),
                "文章摘要": record.get("description_wechat", "") or record.get("snippet_sogou", ""),
                "发现查询词": record.get("discovery_queries", ""),
                "核验状态": record.get("resolution_status", ""),
                "抓取说明": "永久标识链接由微信官方文章的__biz、mid、idx构成，可能触发微信安全验证；抓取时直达链接含时效签名，可能过期。",
            }
        )

    with (OUT / "观澜Horizon_已核验公众号文章链接.csv").open("w", encoding="utf-8-sig", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=verified_headers)
        writer.writeheader()
        writer.writerows(verified_rows)

    unresolved_headers = [
        "文章标题", "发布日期", "公众号名称", "搜狗索引链接", "外部来源链接", "状态", "错误/说明", "发现查询词",
    ]
    unresolved_rows = []
    for record in unresolved:
        unresolved_rows.append(
            {
                "文章标题": record.get("title_wechat") or record.get("title_sogou", ""),
                "发布日期": record.get("publish_date") or record.get("date_sogou", "")[:10],
                "公众号名称": record.get("nickname") or record.get("author_sogou", ACCOUNT),
                "搜狗索引链接": record.get("sogou_href", ""),
                "外部来源链接": record.get("external_source_url", ""),
                "状态": record.get("resolution_status", ""),
                "错误/说明": record.get("resolution_error", "") or record.get("external_source_note", ""),
                "发现查询词": record.get("discovery_queries", ""),
            }
        )
    with (OUT / "观澜Horizon_未解析或外部引用文章.csv").open("w", encoding="utf-8-sig", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=unresolved_headers)
        writer.writeheader()
        writer.writerows(unresolved_rows)

    search_headers = ["查询词", "页码", "HTTP状态", "结果数", "精确账号结果数", "是否验证码", "页面标题", "错误"]
    with (OUT / "观澜Horizon_检索日志.csv").open("w", encoding="utf-8-sig", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=search_headers)
        writer.writeheader()
        for log in search_logs:
            writer.writerow(
                {
                    "查询词": log.get("query", ""),
                    "页码": log.get("page", ""),
                    "HTTP状态": log.get("status", ""),
                    "结果数": log.get("result_count", ""),
                    "精确账号结果数": log.get("target_count", ""),
                    "是否验证码": log.get("captcha", ""),
                    "页面标题": log.get("title", ""),
                    "错误": log.get("error", ""),
                }
            )

    summary = {
        "account": ACCOUNT,
        "expected_biz": EXPECTED_BIZ,
        "expected_user_name": EXPECTED_USER_NAME,
        "generated_at": datetime.now(tz=SHANGHAI).isoformat(),
        "query_count": len(QUERY_SPECS),
        "search_page_count": len(search_logs),
        "unique_sogou_rows": len(raw_records),
        "verified_unique_articles": len(verified),
        "unresolved_or_external_rows": len(unresolved),
        "earliest_verified_date": min((row.get("publish_date") for row in verified if row.get("publish_date")), default=""),
        "latest_verified_date": max((row.get("publish_date") for row in verified if row.get("publish_date")), default=""),
        "profile_history_status": "微信历史页和getmsg分页接口返回no session/请在微信客户端打开，无法证明已覆盖删除或未被公开搜索索引的历史文章。",
        "known_external_unresolved": external_unresolved,
        "verified_rows": verified_rows,
        "unresolved_rows": unresolved_rows,
        "search_logs": search_logs,
    }
    (OUT / "观澜Horizon_文章链接抓取汇总.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({key: summary[key] for key in ["query_count", "search_page_count", "unique_sogou_rows", "verified_unique_articles", "unresolved_or_external_rows", "earliest_verified_date", "latest_verified_date"]}, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
