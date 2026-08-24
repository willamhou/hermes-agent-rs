import json
import re
import time
from pathlib import Path
from urllib.parse import urljoin, urlparse

import requests
from bs4 import BeautifulSoup

OUT = Path('pfa_floorplan_discovery')
OUT.mkdir(exist_ok=True)
IMG_DIR = OUT / 'images'
IMG_DIR.mkdir(exist_ok=True)

KEYWORDS = [
    '参观指南', '观展指南', '逛展指南', '逛展攻略', '全馆图', '展位图',
    '展馆图', '展馆平面图', '摊位图', '展位分布', 'floor plan',
    'hall map', 'booth map', 'visitor guide',
]

session = requests.Session()
session.headers.update({
    'User-Agent': 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/140 Safari/537.36',
    'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
})

pages = []
for base in ['https://www.petfairasia.com/news/', 'https://en.petfairasia.com/news/']:
    for num in range(390, 470):
        pages.append(f'{base}{num}.html')
pages += [
    'https://www.petfairasia.com/',
    'https://en.petfairasia.com/',
    'https://www.petfairasia.com/exhibition/PetFairAsia-SUPPLY.html',
    'https://en.petfairasia.com/exhibition/PetFairAsia-SUPPLY.html',
    'https://www.petfairasia.com/visiting-instructions.html',
    'https://en.petfairasia.com/visiting-instructions.html',
]

results = []
seen_images = set()
for idx, url in enumerate(pages, 1):
    try:
        r = session.get(url, timeout=25, allow_redirects=True)
        if r.status_code != 200 or len(r.text) < 300:
            continue
        soup = BeautifulSoup(r.text, 'html.parser')
        title = ' '.join((soup.title.get_text(' ', strip=True) if soup.title else '').split())
        body = ' '.join(soup.get_text(' ', strip=True).split())
        hit_words = [kw for kw in KEYWORDS if kw.lower() in (title + ' ' + body).lower()]
        images = []
        for img in soup.find_all('img'):
            src = img.get('data-original') or img.get('data-src') or img.get('src') or ''
            if not src:
                continue
            full = urljoin(r.url, src)
            alt = ' '.join((img.get('alt') or '').split())
            item = {
                'url': full,
                'alt': alt,
                'width': img.get('width', ''),
                'height': img.get('height', ''),
            }
            images.append(item)
            if hit_words and full not in seen_images and full.startswith('http'):
                seen_images.add(full)
                try:
                    ir = session.get(full, timeout=35)
                    ctype = ir.headers.get('content-type', '')
                    if ir.status_code == 200 and len(ir.content) > 5000 and 'image' in ctype:
                        ext = '.png' if 'png' in ctype else '.jpg'
                        name = f'{len(seen_images):04d}_{Path(urlparse(full).path).stem[:80]}{ext}'
                        (IMG_DIR / name).write_bytes(ir.content)
                        item['saved_as'] = name
                        item['bytes'] = len(ir.content)
                except Exception as exc:
                    item['download_error'] = str(exc)
        if hit_words:
            results.append({
                'url': r.url,
                'status': r.status_code,
                'title': title,
                'keywords': hit_words,
                'body_excerpt': body[:6000],
                'images': images,
            })
    except Exception as exc:
        results.append({'url': url, 'error': str(exc)})
    if idx % 30 == 0:
        print(f'checked {idx}/{len(pages)}, matches={len(results)}', flush=True)
    time.sleep(0.03)

# Also inspect the known official floor-plan image and all links on the SUPPLY page.
known = 'https://en.petfairasia.com/public/upload/20260330/260330145Z4529.png'
try:
    r = session.get(known, timeout=40)
    if r.status_code == 200:
        (IMG_DIR / 'official_supply_floorplan.png').write_bytes(r.content)
        results.append({'url': known, 'status': r.status_code, 'bytes': len(r.content), 'saved_as': 'official_supply_floorplan.png'})
except Exception as exc:
    results.append({'url': known, 'error': str(exc)})

(OUT / 'discovery.json').write_text(json.dumps(results, ensure_ascii=False, indent=2), encoding='utf-8')
print(json.dumps({'matched_pages': len(results), 'downloaded_images': len(list(IMG_DIR.glob('*')))}, ensure_ascii=False))
