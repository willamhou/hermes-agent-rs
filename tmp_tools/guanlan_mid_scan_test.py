from __future__ import annotations

import json
import re
import time
from pathlib import Path
from urllib.parse import urlencode

import requests
from bs4 import BeautifulSoup

OUT = Path('guanlan_mid_scan_test')
OUT.mkdir(exist_ok=True)
BIZ = 'Mzk5MDA5NjY1OA=='
USER_NAME = 'gh_530fbfb1dbd0'
NICKNAME = '观澜Horizon'
UA = 'Mozilla/5.0 (Linux; Android 13; Pixel 7 Pro Build/TQ3A.230805.001; wv) AppleWebKit/537.36 Version/4.0 Chrome/112.0.0.0 Mobile Safari/537.36 MicroMessenger/8.0.47.2560(0x28002F37) WeChat/arm64 Weixin NetType/WIFI Language/zh_CN ABI/arm64'

MIDS = [2247485640,2247485641,2247485642,2247485643,2247485644,2247486659,2247486660,2247486661,2247486662]

PATTERNS = {
    'biz': [r"var\s+biz\s*=\s*['\"]([^'\"]+)", r'__biz=([A-Za-z0-9_=\-]+)'],
    'mid': [r"var\s+mid\s*=\s*['\"]?(\d+)", r'(?:mid|appmsgid)=([0-9]+)'],
    'idx': [r"var\s+idx\s*=\s*['\"]?(\d+)", r'idx=([0-9]+)'],
    'ct': [r"var\s+ct\s*=\s*['\"]?(\d+)"],
    'nickname': [r"var\s+nickname\s*=\s*htmlDecode\(['\"]([^'\"]+)", r"var\s+nickname\s*=\s*['\"]([^'\"]+)"],
    'user_name': [r"var\s+user_name\s*=\s*['\"]([^'\"]+)"],
    'msg_title': [r"var\s+msg_title\s*=\s*htmlDecode\(['\"]([^'\"]+)", r"var\s+msg_title\s*=\s*['\"]([^'\"]+)"],
}

def first(text, pats):
    for p in pats:
        m=re.search(p,text)
        if m:
            return m.group(1)
    return ''

s=requests.Session()
s.headers.update({'User-Agent':UA,'Accept-Language':'zh-CN,zh;q=0.9','Accept':'text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8'})
rows=[]
for mid in MIDS:
    url='https://mp.weixin.qq.com/s?'+urlencode({'__biz':BIZ,'mid':mid,'idx':1})
    try:
        r=s.get(url,timeout=35,allow_redirects=True)
        text=r.text
        fields={k:first(text,v) for k,v in PATTERNS.items()}
        soup=BeautifulSoup(text,'html.parser')
        row={
            'requested_mid':mid,'url':url,'status':r.status_code,'final_url':r.url,'bytes':len(r.content),
            **fields,
            'html_title':soup.title.get_text(' ',strip=True) if soup.title else '',
            'excerpt':' '.join(soup.get_text(' ',strip=True).split())[:500],
            'target_match': fields['biz']==BIZ and fields['user_name']==USER_NAME and fields['nickname']==NICKNAME,
            'environment_error': any(x in text for x in ['环境异常','访问过于频繁','请在微信客户端打开','该内容已被发布者删除','此内容因违规无法查看']),
        }
        (OUT/f'{mid}.html').write_bytes(r.content)
    except Exception as e:
        row={'requested_mid':mid,'url':url,'error':repr(e)}
    rows.append(row)
    print(json.dumps(row,ensure_ascii=False),flush=True)
    time.sleep(0.7)

(OUT/'summary.json').write_text(json.dumps(rows,ensure_ascii=False,indent=2),encoding='utf-8')
