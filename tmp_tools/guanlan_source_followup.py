from __future__ import annotations
import requests,json,re,html,time,csv
from pathlib import Path
from urllib.parse import urlencode,urljoin,urlparse,parse_qs,unquote
from bs4 import BeautifulSoup
from datetime import datetime,timezone,timedelta
O=Path('guanlan_source_followup');O.mkdir(exist_ok=True)
B='Mzk5MDA5NjY1OA=='
U='gh_530fbfb1dbd0'
TZ=timezone(timedelta(hours=8))
S=requests.Session();S.headers.update({'User-Agent':'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/140.0.0.0 Safari/537.36','Accept-Language':'zh-CN,zh;q=0.9'})
logs=[];found={};raw_articles=[];wechat_blocked=False

def fetch(url,name):
    try:
        r=S.get(url,timeout=(10,30),allow_redirects=True)
        r.encoding='utf8' if 'utf-8' in r.headers.get('content-type','').lower() or b'charset=utf-8' in r.content[:3000].lower() else r.encoding
        (O/(name+'.html')).write_bytes(r.content)
        p=BeautifulSoup(r.text,'html.parser');body=p.get_text(' ',strip=True)
        blocked=r.status_code in (403,429) or any(x in r.url for x in ['antispider','wappoc_appmsgcaptcha']) or (len(r.content)<50000 and any(x in body for x in ['请输入验证码','访问过于频繁','请在微信客户端打开','环境异常']))
        rec={'name':name,'url':url,'final_url':r.url,'status':r.status_code,'bytes':len(r.content),'blocked':blocked,'text':body[:5000]}
        logs.append(rec);print(name,r.status_code,len(r.content),'blocked',blocked,flush=True)
        return r,p,rec
    except Exception as e:
        logs.append({'name':name,'url':url,'error':str(e)});return None

def first(t,pat):
    m=re.search(pat,t);return html.unescape(m.group(1)) if m else ''

def article(url,name,source):
    global wechat_blocked
    a=fetch(url,name)
    if not a:return
    r,p,rec=a
    if rec['blocked']:
        wechat_blocked=True;return
    t=r.text
    d={'url':url,'final_url':r.url,'source':source,'name':name}
    for k,pat in {
        'biz':r'var\s+biz\s*=\s*[\'\"]([^\'\"]+)',
        'mid':r'var\s+mid\s*=\s*[\'\"]?(\d+)',
        'idx':r'var\s+idx\s*=\s*[\'\"]?(\d+)',
        'sn':r'var\s+sn\s*=\s*[\'\"]([^\'\"]*)',
        'ct':r'var\s+ct\s*=\s*[\'\"]?(\d+)',
        'nickname':r'var\s+nickname\s*=\s*(?:htmlDecode\()?\s*[\'\"]([^\'\"]+)',
        'user_name':r'var\s+user_name\s*=\s*[\'\"]([^\'\"]+)',
    }.items():d[k]=first(t,pat)
    title=p.select_one('#activity-name') or p.select_one('meta[property="og:title"]')
    d['title']=title.get('content',title.get_text(' ',strip=True)) if title else ''
    d['date']=datetime.fromtimestamp(int(d['ct']),TZ).isoformat() if d['ct'].isdigit() else ''
    d['account_verified']=d['biz']==B and d['user_name']==U
    d['album']=first(t,r'var\s+albumInfo\s*=\s*[\'\"](.*?)[\'\"];')
    d['all_links']=[]
    for n in p.find_all(True):
        for attr in ['href','data-link','data-url','data-href']:
            v=n.get(attr)
            if v and ('mp.weixin.qq.com/' in v or '/mp/appmsgalbum' in v):d['all_links'].append({'text':n.get_text(' ',strip=True)[:100],'url':urljoin(r.url,html.unescape(v))})
    d['album_ids']=list(set(re.findall(r'(?:album_id|albumId|albumid)[\"\s:=]+[\"\s]*(\d{5,})',t)))
    raw_articles.append(d)
    print('ARTICLE',d['date'],d['title'],d['account_verified'],'links',len(d['all_links']),'albums',d['album_ids'],flush=True)

original='https://mp.weixin.qq.com/s?__biz=Mzk5MDA5NjY1OA==&mid=2247486038&idx=1&sn=2b99f2258b46f4429153b8a795a59efd'
article(original,'original_source','https://post.smzdm.com/p/arzw6pgx/')
time.sleep(3)
# Read only published public search indexes. Stop rather than retry a CAPTCHA.
queries=['"gh_530fbfb1dbd0"','"Mzk5MDA5NjY1OA"','"观澜Horizon"']
stop_search=False
for qi,q in enumerate(queries):
    url='https://weixin.sogou.com/weixin?'+urlencode({'type':2,'query':q,'ie':'utf8'})
    for pi in range(1,5):
        a=fetch(url,f'search_{qi}_{pi}');time.sleep(3)
        if not a:break
        r,p,rec=a
        if rec['blocked']:stop_search=True;break
        for li in p.select('ul.news-list li'):
            a=li.select_one('h3 a[href]');au=li.select_one('span.all-time-y2')
            if a is None or au is None:continue
            author=au.get_text(' ',strip=True)
            if author!='观澜Horizon' and qi!=0:continue
            title=a.get_text(' ',strip=True)
            key=li.get('d') or title
            if key not in found:found[key]={'title':title,'author':author,'url':urljoin(r.url,a['href']),'source':r.url,'text':li.get_text(' ',strip=True)}
        nxt=p.select_one('#sogou_next[href]')
        if not nxt:break
        url=urljoin(r.url,nxt['href'])
    if stop_search:break
# Resolve each discovered article exactly once, preserving successful full URLs.
for i,row in enumerate(found.values()):
    if wechat_blocked:break
    if i>=50:break
    a=fetch(row['url'],f'redirect_{i}');time.sleep(2)
    if not a:continue
    r,p,rec=a
    if rec['blocked']:break
    u=''.join(re.findall(r'url\s*\+=\s*[\'\"]([^\'\"]*)[\'\"]\s*;',r.content.decode('gbk',errors='ignore'))).replace('@','')
    if u.startswith('https://mp.weixin.qq.com/'):
        row['wechat_url']=u
        article(u,f'article_{i}',row['source']);time.sleep(2)
# Public source indexes: exact account search; no login or subscription access.
for name,url in [
    ('smzdm_search','https://search.smzdm.com/?'+urlencode({'c':'post','s':'观澜Horizon','v':'b'})),
    ('wemp_search','https://wemp.app/search?'+urlencode({'q':'观澜Horizon'})),
    ('qingbo_search','https://www.gsdata.cn/query/wx?'+urlencode({'q':'观澜Horizon'})),
    ('newrank_search','https://www.newrank.cn/public/info/search.html?'+urlencode({'value':'观澜Horizon'})),
]:
    fetch(url,name);time.sleep(3)
summary={'generated_at':datetime.now(TZ).isoformat(),'expected_biz':B,'expected_user_name':U,'logs':logs,'searches':list(found.values()),'articles':raw_articles}
(O/'summary.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2),encoding='utf8')
print('FINAL','indexed',len(found),'fetched',len(raw_articles),'verified',sum(d['account_verified'] for d in raw_articles),flush=True)
