import csv
import json
import random
import re
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from urllib.parse import urljoin

import requests
from bs4 import BeautifulSoup

BASE_URL = "https://online.petfairasia.com"
SEED_PATH = Path("pet_seed/all_exhibitors_enriched.json")
OUT_DIR = Path("pfa_pet_supplies_full_brands")
OUT_DIR.mkdir(parents=True, exist_ok=True)
MAX_WORKERS = 12
_tls = threading.local()


def clean(value):
    if value is None:
        return ""
    return re.sub(r"\s+", " ", str(value).replace("\u200c", "").replace("\ufeff", " ")).strip()


def natural_key(value):
    return [int(part) if part.isdigit() else part.lower() for part in re.split(r"(\d+)", value or "")]


def get_session():
    session = getattr(_tls, "session", None)
    if session is None:
        session = requests.Session()
        session.headers.update(
            {
                "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/140 Safari/537.36",
                "Accept-Language": "en-US,en;q=0.9",
                "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                "Referer": "https://online.petfairasia.com/en/showroom-2026/institutions",
            }
        )
        _tls.session = session
    return session


def parse_brand_records(html):
    soup = BeautifulSoup(html, "html.parser")
    records = []
    seen_urls = set()
    for anchor in soup.select('a[href*="/showroom-2026/brands/"]'):
        href = anchor.get("href", "")
        if not href:
            continue
        absolute_url = urljoin(BASE_URL, href)
        if absolute_url in seen_urls:
            continue
        seen_urls.add(absolute_url)
        card = anchor.find_parent("div", class_=lambda classes: classes and "como-card" in classes)
        primary = card.select_one(".como-card__primary") if card else None
        name = clean(primary.get_text(" ", strip=True)) if primary else ""
        if not name and card:
            action = card.select_one(".mdc-card__primary-action")
            impression = action.get("data-impression", "") if action else ""
            match = re.search(r"impressed como_brand_information:\s*(.+?)\"", impression)
            name = clean(match.group(1)) if match else ""
        if name:
            records.append({"name": name, "url": absolute_url})
    return records


def unique_brand_names(records):
    names = []
    seen = set()
    for record in records:
        key = clean(record["name"]).casefold()
        if key and key not in seen:
            seen.add(key)
            names.append(clean(record["name"]))
    return names


def visible_brand_records(row):
    names = [clean(value) for value in row.get("brands_raw", []) if clean(value)]
    links = [clean(value) for value in row.get("品牌详情页", "").split("；") if clean(value)]
    records = []
    for index, name in enumerate(names):
        records.append({"name": name, "url": links[index] if index < len(links) else ""})
    return records


def enrich_row(row):
    detail_url = clean(row.get("官方详情页", ""))
    related_url = detail_url.rstrip("/") + "/related-brands"
    last_error = ""
    for attempt in range(1, 6):
        try:
            response = get_session().get(related_url, timeout=45, allow_redirects=True)
            if response.status_code == 200 and len(response.text) > 12000:
                records = parse_brand_records(response.text)
                names = unique_brand_names(records)
                enriched = dict(row)
                enriched["品牌"] = "；".join(names)
                enriched["品牌数"] = len(names)
                enriched["官方品牌记录数"] = len(records)
                enriched["品牌详情页"] = "；".join(record["url"] for record in records if record["url"])
                enriched["品牌数据页"] = related_url
                enriched["品牌抓取状态"] = "成功" if records else "成功（官方未披露品牌）"
                enriched["品牌抓取错误"] = ""
                enriched["full_brand_records"] = records
                return enriched
            last_error = f"HTTP {response.status_code}, length={len(response.text)}"
        except Exception as exc:
            last_error = f"{type(exc).__name__}: {exc}"
        time.sleep(min(12, 0.8 * (2 ** (attempt - 1))) + random.random())

    fallback = visible_brand_records(row)
    names = unique_brand_names(fallback)
    enriched = dict(row)
    enriched["品牌"] = "；".join(names)
    enriched["品牌数"] = len(names)
    enriched["官方品牌记录数"] = len(fallback)
    enriched["品牌详情页"] = "；".join(record["url"] for record in fallback if record["url"])
    enriched["品牌数据页"] = related_url
    enriched["品牌抓取状态"] = "失败，已回退至详情首页可见品牌"
    enriched["品牌抓取错误"] = last_error
    enriched["full_brand_records"] = fallback
    return enriched


def main():
    all_rows = json.loads(SEED_PATH.read_text(encoding="utf-8"))
    pet_rows = [row for row in all_rows if row.get("is_pet_supplies")]
    results = []
    start = time.time()
    with ThreadPoolExecutor(max_workers=MAX_WORKERS) as executor:
        futures = {executor.submit(enrich_row, row): row for row in pet_rows}
        for index, future in enumerate(as_completed(futures), 1):
            results.append(future.result())
            if index % 100 == 0 or index == len(pet_rows):
                elapsed = time.time() - start
                failures = sum(row["品牌抓取状态"].startswith("失败") for row in results)
                branded = sum(bool(row["品牌"]) for row in results)
                over_four = sum(int(row["官方品牌记录数"]) > 4 for row in results)
                print(
                    f"progress={index}/{len(pet_rows)} elapsed={elapsed:.1f}s branded={branded} over_four={over_four} failures={failures}",
                    flush=True,
                )

    results.sort(key=lambda row: (natural_key(row.get("展位号", "")), row.get("公司名称", ""), row.get("官方企业ID", "")))
    headers = [
        "序号",
        "公司名称",
        "英文名称",
        "品牌",
        "品牌数",
        "官方品牌记录数",
        "品牌详情页",
        "品牌数据页",
        "品牌抓取状态",
        "品牌抓取错误",
        "展位号",
        "展馆",
        "国家/地区",
        "官方企业ID",
        "官方企业分类",
        "宠物用品相关分类",
        "宠物用品相关产品",
        "相关产品详情页",
        "业务性质",
        "标签",
        "筛选依据",
        "官方详情页",
        "Logo/图片",
        "来源页码",
        "详情抓取状态",
        "详情抓取错误",
    ]
    csv_path = OUT_DIR / "2026上海亚洲宠物展_宠物用品供应商_品牌完整版.csv"
    with csv_path.open("w", encoding="utf-8-sig", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=headers, extrasaction="ignore")
        writer.writeheader()
        for index, row in enumerate(results, 1):
            writer.writerow({"序号": index, **row})

    (OUT_DIR / "pet_supplies_full_brands.json").write_text(
        json.dumps(results, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    failed = [row for row in results if row["品牌抓取状态"].startswith("失败")]
    branded = [row for row in results if row["品牌"]]
    summary = {
        "pet_supplies_records": len(results),
        "brand_page_success": len(results) - len(failed),
        "brand_page_failures": len(failed),
        "companies_with_official_brand": len(branded),
        "companies_without_official_brand": len(results) - len(branded),
        "companies_with_more_than_4_official_brand_records": sum(
            int(row["官方品牌记录数"]) > 4 for row in results
        ),
        "max_unique_brand_names_per_company": max((int(row["品牌数"]) for row in results), default=0),
        "max_official_brand_records_per_company": max(
            (int(row["官方品牌记录数"]) for row in results), default=0
        ),
        "filter_rule": "Official company categories or a visible official product card contains the exact category 'Pet Supplies'.",
        "brand_rule": "Brand names are taken from each exhibitor's official Related brands page; duplicate names are collapsed in the 品牌 column while official records and links are retained.",
        "errors": [
            {
                "company": row["公司名称"],
                "official_id": row["官方企业ID"],
                "url": row["品牌数据页"],
                "error": row["品牌抓取错误"],
            }
            for row in failed
        ],
    }
    (OUT_DIR / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2), flush=True)


if __name__ == "__main__":
    main()
