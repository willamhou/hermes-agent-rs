import json
import re
import time
from pathlib import Path

import requests
from bs4 import BeautifulSoup

OUT = Path('pfa_detail_probe')
OUT.mkdir(exist_ok=True)

URLS = [
    'https://online.petfairasia.com/en/showroom-2026/institutions/800edf',
    'https://online.petfairasia.com/en/showroom-2026/institutions/02echk0',
    'https://online.petfairasia.com/en/showroom-2026/institutions/287hcj',
]

session = requests.Session()
session.headers.update({
    'User-Agent': 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/140 Safari/537.36',
    'Accept-Language': 'en-US,en;q=0.9',
    'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8',
})

summary = []
for idx, url in enumerate(URLS, 1):
    resp = session.get(url, timeout=45, allow_redirects=True)
    html = resp.text
    (OUT / f'{idx}.html').write_text(html, encoding='utf-8')
    soup = BeautifulSoup(html, 'html.parser')
    links = []
    for a in soup.find_all('a', href=True):
        href = a.get('href', '')
        text = ' '.join(a.get_text(' ', strip=True).split())
        if '/brands/' in href or '/products/' in href:
            links.append({'href': href, 'text': text})
    headings = [
        ' '.join(h.get_text(' ', strip=True).split())
        for h in soup.find_all(re.compile('^h[1-6]$'))
    ]
    body_text = ' '.join(soup.get_text(' ', strip=True).split())
    summary.append({
        'url': url,
        'status': resp.status_code,
        'final_url': resp.url,
        'length': len(html),
        'title': soup.title.get_text(' ', strip=True) if soup.title else '',
        'headings': headings[:100],
        'brand_product_links': links[:300],
        'body_excerpt': body_text[:12000],
    })
    time.sleep(1)

(OUT / 'summary.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding='utf-8')
print(json.dumps(summary, ensure_ascii=False, indent=2)[:20000])
