import csv
import json
import random
import re
import threading
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from urllib.parse import urljoin

import requests
from bs4 import BeautifulSoup

BASE_URL = "https://online.petfairasia.com"
SEED_PATH = Path("seed_artifact/petfairasia_2026_exhibitors_raw.json")
OUT_DIR = Path("pfa_pet_supplies_output")
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


def extract_chip_group(soup, heading):
    node = soup.find(string=lambda value: value and clean(value).upper() == heading.upper())
    if not node:
        return []
    container = node.parent.find_parent("div", class_="flex-space__item") or node.parent.parent
    values = [clean(item.get_text(" ", strip=True)) for item in container.select(".mdc-chip__text")]
    result = []
    for value in values:
        if value and value not in result:
            result.append(value)
    return result


def extract_brands(soup):
    brands = []
    links = []
    seen = set()
    for anchor in soup.select('a[href*="/showroom-2026/brands/"]'):
        href = anchor.get("href", "")
        if not href or href in seen:
            continue
        seen.add(href)
        card = anchor.find_parent("div", class_=lambda classes: classes and "como-card" in classes)
        if not card:
            continue
        primary = card.select_one(".como-card__primary")
        name = clean(primary.get_text(" ", strip=True)) if primary else ""
        if not name:
            action = card.select_one(".mdc-card__primary-action")
            impression = action.get("data-impression", "") if action else ""
            match = re.search(r"impressed como_brand_information:\s*(.+?)\"", impression)
            name = clean(match.group(1)) if match else ""
        if name:
            brands.append(name)
            links.append(urljoin(BASE_URL, href))
    return brands, links


def extract_products(soup):
    products = []
    seen = set()
    for anchor in soup.select('a[href*="/showroom-2026/products/"]'):
        href = anchor.get("href", "")
        if not href or href in seen:
            continue
        seen.add(href)
        card = anchor.find_parent("div", class_=lambda classes: classes and "como-card" in classes)
        if not card:
            continue
        title_node = card.select_one(".como-card__title")
        subtitle_node = card.select_one(".como-card__subtitle")
        title = clean(title_node.get_text(" ", strip=True)) if title_node else ""
        subtitle = clean(subtitle_node.get_text(" ", strip=True)) if subtitle_node else ""
        categories = [clean(part) for part in subtitle.split(",") if clean(part)]
        products.append(
            {
                "title": title,
                "categories": categories,
                "url": urljoin(BASE_URL, href),
            }
        )
    return products


def extract_english_name(soup):
    heading = soup.select_one("main h1") or soup.find("h1")
    if heading:
        return clean(heading.get_text(" ", strip=True))
    title = soup.title.get_text(" ", strip=True) if soup.title else ""
    match = re.match(r"About\s*\|\s*(.*?)\s*\|\s*Pet Fair Asia", title)
    return clean(match.group(1)) if match else ""


def fetch_detail(seed):
    url = seed.get("detail_url") or f"{BASE_URL}/en/showroom-2026/institutions/{seed['raw']['id']['value']}"
    last_error = ""
    for attempt in range(1, 6):
        try:
            response = get_session().get(url, timeout=45, allow_redirects=True)
            if response.status_code == 200 and len(response.text) > 15000:
                soup = BeautifulSoup(response.text, "html.parser")
                categories = extract_chip_group(soup, "PRODUCT/SERVICE CATEGORIES")
                business_nature = extract_chip_group(soup, "BUSINESS NATURE")
                tags = extract_chip_group(soup, "TAGS")
                brands, brand_links = extract_brands(soup)
                products = extract_products(soup)
                product_pet_supply = [
                    item for item in products if any(cat.casefold() == "pet supplies" for cat in item["categories"])
                ]
                company_match = any(cat.casefold() == "pet supplies" for cat in categories)
                product_match = bool(product_pet_supply)
                pet_supply_subcategories = []
                for category in categories:
                    if category.casefold() != "pet supplies" and category not in pet_supply_subcategories:
                        pet_supply_subcategories.append(category)
                for product in product_pet_supply:
                    for category in product["categories"]:
                        if category.casefold() != "pet supplies" and category not in pet_supply_subcategories:
                            pet_supply_subcategories.append(category)
                basis = []
                if company_match:
                    basis.append("企业分类含 Pet Supplies")
                if product_match:
                    basis.append("产品分类含 Pet Supplies")
                raw = seed.get("raw", {})
                official_id = clean(raw.get("id", {}).get("value", ""))
                return {
                    "公司名称": clean(seed.get("company", "")),
                    "英文名称": extract_english_name(soup),
                    "品牌": "；".join(brands),
                    "品牌数": len(brands),
                    "品牌详情页": "；".join(brand_links),
                    "展位号": clean(seed.get("booth", "")),
                    "展馆": clean(seed.get("hall", "")) or clean(seed.get("booth", "")).split(" / ")[0],
                    "国家/地区": clean(seed.get("country", "")),
                    "官方企业ID": official_id,
                    "官方企业分类": "；".join(categories),
                    "宠物用品相关分类": "；".join(pet_supply_subcategories),
                    "宠物用品相关产品": "；".join(item["title"] for item in product_pet_supply if item["title"]),
                    "相关产品详情页": "；".join(item["url"] for item in product_pet_supply),
                    "业务性质": "；".join(business_nature),
                    "标签": "；".join(tags),
                    "筛选依据": "；".join(basis),
                    "官方详情页": url,
                    "Logo/图片": clean(seed.get("logo", "")),
                    "来源页码": seed.get("page", ""),
                    "详情抓取状态": "成功",
                    "详情抓取错误": "",
                    "is_pet_supplies": company_match or product_match,
                    "company_categories": categories,
                    "brands_raw": brands,
                    "products_raw": products,
                }
            last_error = f"HTTP {response.status_code}, length={len(response.text)}"
        except Exception as exc:
            last_error = f"{type(exc).__name__}: {exc}"
        time.sleep(min(12, 0.8 * (2 ** (attempt - 1))) + random.random())
    raw = seed.get("raw", {})
    return {
        "公司名称": clean(seed.get("company", "")),
        "英文名称": "",
        "品牌": "",
        "品牌数": 0,
        "品牌详情页": "",
        "展位号": clean(seed.get("booth", "")),
        "展馆": clean(seed.get("hall", "")) or clean(seed.get("booth", "")).split(" / ")[0],
        "国家/地区": clean(seed.get("country", "")),
        "官方企业ID": clean(raw.get("id", {}).get("value", "")),
        "官方企业分类": "",
        "宠物用品相关分类": "",
        "宠物用品相关产品": "",
        "相关产品详情页": "",
        "业务性质": "",
        "标签": "",
        "筛选依据": "",
        "官方详情页": url,
        "Logo/图片": clean(seed.get("logo", "")),
        "来源页码": seed.get("page", ""),
        "详情抓取状态": "失败",
        "详情抓取错误": last_error,
        "is_pet_supplies": False,
        "company_categories": [],
        "brands_raw": [],
        "products_raw": [],
    }


def main():
    seed_obj = json.loads(SEED_PATH.read_text(encoding="utf-8"))
    seeds = seed_obj["records"]
    results = []
    start = time.time()
    with ThreadPoolExecutor(max_workers=MAX_WORKERS) as executor:
        future_map = {executor.submit(fetch_detail, seed): seed for seed in seeds}
        for index, future in enumerate(as_completed(future_map), 1):
            results.append(future.result())
            if index % 100 == 0 or index == len(seeds):
                elapsed = time.time() - start
                failures = sum(row["详情抓取状态"] != "成功" for row in results)
                matches = sum(bool(row["is_pet_supplies"]) for row in results)
                print(f"progress={index}/{len(seeds)} elapsed={elapsed:.1f}s matches={matches} failures={failures}", flush=True)

    results.sort(key=lambda row: (natural_key(row["展位号"]), row["公司名称"], row["官方企业ID"]))
    pet_rows = [row for row in results if row["is_pet_supplies"]]
    headers = [
        "序号",
        "公司名称",
        "英文名称",
        "品牌",
        "品牌数",
        "品牌详情页",
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
    csv_path = OUT_DIR / "2026上海亚洲宠物展_宠物用品供应商_含品牌.csv"
    with csv_path.open("w", encoding="utf-8-sig", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=headers, extrasaction="ignore")
        writer.writeheader()
        for index, row in enumerate(pet_rows, 1):
            writer.writerow({"序号": index, **row})

    all_json_path = OUT_DIR / "all_exhibitors_enriched.json"
    all_json_path.write_text(json.dumps(results, ensure_ascii=False, indent=2), encoding="utf-8")
    errors = [row for row in results if row["详情抓取状态"] != "成功"]
    brand_coverage = sum(bool(row["品牌"]) for row in pet_rows)
    category_counter = Counter()
    for row in pet_rows:
        category_counter.update(row["宠物用品相关分类"].split("；") if row["宠物用品相关分类"] else [])
    summary = {
        "source_records": len(seeds),
        "detail_success": len(results) - len(errors),
        "detail_failures": len(errors),
        "pet_supplies_records": len(pet_rows),
        "pet_supplies_with_official_brand": brand_coverage,
        "pet_supplies_without_official_brand": len(pet_rows) - brand_coverage,
        "filter_rule": "Include when the official company categories or a visible official product card contains the exact category 'Pet Supplies'.",
        "top_related_categories": category_counter.most_common(50),
        "errors": [
            {
                "company": row["公司名称"],
                "official_id": row["官方企业ID"],
                "url": row["官方详情页"],
                "error": row["详情抓取错误"],
            }
            for row in errors
        ],
    }
    (OUT_DIR / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2), flush=True)


if __name__ == "__main__":
    main()
