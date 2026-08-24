from __future__ import annotations

import json
import re
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from urllib.parse import urljoin, urlparse

import requests
from bs4 import BeautifulSoup

OUT = Path('pfa_floorplan_target')
IMG = OUT / 'images'
OUT.mkdir(exist_ok=True)
IMG.mkdir(exist_ok=True)

HEADERS = {
    'User-Agent': 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/140 Safari/537.36',
    'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
}

URLS = [f'https://www.petfairasia.com/news/{i}.html' for i in range(430, 456)] + [
    'https://www.10100.com/article/149305789',
    'https://www.petfairasia.com/news/378.html',
]


def fetch_page(url: str) -> dict:
    s = requests.Session(); s.headers.update(HEADERS)
    try:
        r = s.get(url, timeout=25, allow_redirects=True)
        soup = BeautifulSoup(r.text, 'html.parser')
        title = ' '.join((soup.title.get_text(' ', strip=True) if soup.title else '').split())
        h1 = ' '.join((soup.find('h1').get_text(' ', strip=True) if soup.find('h1') else '').split())
        body = ' '.join(soup.get_text(' ', strip=True).split())
        images = []
        for img in soup.find_all('img'):
            src = img.get('data-original') or img.get('data-src') or img.get('src') or ''
            if not src:
                continue
            full = urljoin(r.url, src)
            images.append({
                'url': full,
                'alt': ' '.join((img.get('alt') or '').split()),
                'width': img.get('width', ''),
                'height': img.get('height', ''),
            })
        return {
            'requested_url': url,
            'url': r.url,
            'status': r.status_code,
            'length': len(r.text),
            'title': title,
            'h1': h1,
            'body_excerpt': body[:3000],
            'images': images,
        }
    except Exception as exc:
        return {'requested_url': url, 'error': f'{type(exc).__name__}: {exc}'}


with ThreadPoolExecutor(max_workers=12) as ex:
    results = list(ex.map(fetch_page, URLS))

selected = []
for row in results:
    text = f"{row.get('title','')} {row.get('h1','')} {row.get('body_excerpt','')}"
    if any(k in text for k in ['全馆展位图', '展位分布图', '展位图', '观展指南']) or '149305789' in row.get('requested_url',''):
        selected.append(row)

seen = set()
downloads = []
session = requests.Session(); session.headers.update(HEADERS)
for page_index, row in enumerate(selected, 1):
    for image_index, item in enumerate(row.get('images', []), 1):
        url = item['url']
        if url in seen or not url.startswith('http'):
            continue
        seen.add(url)
        try:
            r = session.get(url, timeout=35, allow_redirects=True)
            ctype = r.headers.get('content-type', '').lower()
            if r.status_code != 200 or len(r.content) < 3000 or 'image' not in ctype:
                continue
            ext = '.png' if 'png' in ctype else ('.webp' if 'webp' in ctype else '.jpg')
            stem = re.sub(r'[^0-9A-Za-z._-]+', '_', Path(urlparse(r.url).path).stem)[:80]
            filename = f'p{page_index:02d}_i{image_index:03d}_{stem}{ext}'
            (IMG / filename).write_bytes(r.content)
            entry = dict(item)
            entry.update({'saved_as': filename, 'bytes': len(r.content), 'content_type': ctype, 'source_page': row.get('url')})
            downloads.append(entry)
        except Exception as exc:
            downloads.append({'url': url, 'source_page': row.get('url'), 'error': str(exc)})

(OUT / 'all_pages.json').write_text(json.dumps(results, ensure_ascii=False, indent=2), encoding='utf-8')
(OUT / 'selected_pages.json').write_text(json.dumps(selected, ensure_ascii=False, indent=2), encoding='utf-8')
(OUT / 'downloads.json').write_text(json.dumps(downloads, ensure_ascii=False, indent=2), encoding='utf-8')
print(json.dumps({
    'pages': len(results),
    'selected_pages': [{'url': r.get('url'), 'title': r.get('title'), 'h1': r.get('h1'), 'images': len(r.get('images', []))} for r in selected],
    'downloaded_images': len([x for x in downloads if x.get('saved_as')]),
}, ensure_ascii=False, indent=2))
