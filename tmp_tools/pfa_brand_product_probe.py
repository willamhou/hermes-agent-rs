import json
import re
from pathlib import Path
from urllib.parse import urljoin

import requests
from bs4 import BeautifulSoup

BASE = "https://online.petfairasia.com"
OUT = Path("pfa_brand_product_probe")
OUT.mkdir(exist_ok=True)
URLS = [
    "https://online.petfairasia.com/en/showroom-2026/brands/94fdaj7",
    "https://online.petfairasia.com/zh-cn/showroom-2026/brands/94fdaj7",
    "https://online.petfairasia.com/en/showroom-2026/brands/3c0fee2",
    "https://online.petfairasia.com/zh-cn/showroom-2026/brands/3c0fee2",
    "https://online.petfairasia.com/en/showroom-2026/products/b56chi38",
    "https://online.petfairasia.com/zh-cn/showroom-2026/products/b56chi38",
    "https://online.petfairasia.com/en/showroom-2026/institutions/800edf",
    "https://online.petfairasia.com/zh-cn/showroom-2026/institutions/800edf",
]

session = requests.Session()
session.headers.update({
    "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/140 Safari/537.36",
    "Accept-Language": "zh-CN,zh;q=0.9,en;q=0.8",
})

def clean(text):
    return re.sub(r"\s+", " ", text or "").strip()

rows=[]
for i,url in enumerate(URLS,1):
    response=session.get(url,timeout=40)
    html=response.text
    (OUT/f"{i}.html").write_text(html,encoding="utf-8")
    soup=BeautifulSoup(html,"html.parser")
    external=[]
    for a in soup.find_all("a",href=True):
        href=urljoin(BASE,a["href"])
        if href.startswith("http") and "petfairasia.com" not in href and "event-lightning.com" not in href:
            external.append({"href":href,"text":clean(a.get_text(" ",strip=True)),"class":a.get("class",[])})
    links=[]
    for fragment in ["/brands/","/products/","/institutions/"]:
        for a in soup.find_all("a",href=lambda h:h and fragment in h):
            card=a.find_parent("div",class_=lambda c:c and "mdc-card" in c.split())
            primary=card.select_one(".como-card__primary") if card else None
            secondary=card.select(".como-card__secondary div") if card else []
            links.append({
                "type":fragment,
                "href":urljoin(BASE,a.get("href","")),
                "primary":clean(primary.get_text(" ",strip=True) if primary else ""),
                "secondary":[clean(x.get_text(" ",strip=True)) for x in secondary if clean(x.get_text(" ",strip=True))],
            })
    tables=[]
    for table in soup.find_all("table"):
        table_rows=[]
        for tr in table.find_all("tr"):
            th=tr.find("th"); td=tr.find("td")
            if th and td:
                table_rows.append({"key":clean(th.get_text(" ",strip=True)),"value":clean(td.get_text(" ",strip=True)),"hrefs":[urljoin(BASE,a.get("href","")) for a in td.find_all("a",href=True)]})
        if table_rows: tables.append(table_rows)
    headings=[clean(h.get_text(" ",strip=True)) for h in soup.find_all(re.compile("^h[1-6]$"))]
    rows.append({
        "url":url,"status":response.status_code,"length":len(html),
        "title":clean(soup.title.get_text(" ",strip=True) if soup.title else ""),
        "headings":headings,"tables":tables,"external_links":external,"entity_links":links,
        "body_excerpt":clean(soup.get_text(" ",strip=True))[:20000],
    })
(OUT/"summary.json").write_text(json.dumps(rows,ensure_ascii=False,indent=2),encoding="utf-8")
print(json.dumps(rows,ensure_ascii=False,indent=2)[:30000])
