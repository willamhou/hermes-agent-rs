import json
from pathlib import Path
from urllib.parse import urljoin

import requests
from bs4 import BeautifulSoup

BASE = "https://online.petfairasia.com"
OUT = Path("pfa_related_brands_probe")
OUT.mkdir(exist_ok=True)
URLS = [
    "https://online.petfairasia.com/en/showroom-2026/institutions/b65c0e8/related-brands",
    "https://online.petfairasia.com/en/showroom-2026/institutions/02echk0/related-brands",
    "https://online.petfairasia.com/en/showroom-2026/institutions/287hcj/related-brands",
]
headers = {
    "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/140 Safari/537.36",
    "Accept-Language": "en-US,en;q=0.9",
}
summary = []
for index, url in enumerate(URLS, 1):
    response = requests.get(url, headers=headers, timeout=45)
    html = response.text
    (OUT / f"{index}.html").write_text(html, encoding="utf-8")
    soup = BeautifulSoup(html, "html.parser")
    brands = []
    for anchor in soup.select('a[href*="/showroom-2026/brands/"]'):
        card = anchor.find_parent("div", class_=lambda classes: classes and "como-card" in classes)
        primary = card.select_one(".como-card__primary") if card else None
        brands.append(
            {
                "name": " ".join(primary.get_text(" ", strip=True).split()) if primary else "",
                "url": urljoin(BASE, anchor.get("href", "")),
            }
        )
    pager = [
        {
            "text": " ".join(anchor.get_text(" ", strip=True).split()),
            "href": urljoin(BASE, anchor.get("href", "")),
            "class": anchor.get("class", []),
            "rel": anchor.get("rel", []),
        }
        for anchor in soup.select("nav.pager a, .pager a, a[rel=next], a[rel=prev]")
    ]
    summary.append(
        {
            "url": url,
            "status": response.status_code,
            "length": len(html),
            "title": soup.title.get_text(" ", strip=True) if soup.title else "",
            "brands": brands,
            "brand_count": len({item["url"] for item in brands}),
            "pager": pager,
            "body_excerpt": " ".join(soup.get_text(" ", strip=True).split())[:10000],
        }
    )
(OUT / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
print(json.dumps(summary, ensure_ascii=False, indent=2))
