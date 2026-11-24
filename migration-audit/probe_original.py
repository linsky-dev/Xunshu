"""Original 0.17.0 contracts and defects, using only synthetic in-memory records."""
import json
import sqlite3
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from linsearch.live_index import from_entries, RENAME_OLD, RENAME_NEW, DELETE
from linsearch.collection_policy import normalize_collection_policy, accepts, CollectionPolicyError
from linsearch.query import compile_query, match_text
from linsearch.settings import normalize_auto_index_state

results = []
def record(case, expected, actual, kind='defect'):
    results.append(dict(id=case, kind=kind, expected=expected, actual=actual, matches_contract=actual == expected))
def change(frn, parent, name, reason=0x100, attributes=0x20):
    return dict(frn=frn,parent=parent,name=name,reason=reason,attributes=attributes)
def index(paths=False):
    return from_entries([('/synthetic/docs',True,False),('/synthetic/docs/old.txt',False,False)],'/synthetic/',frns=[5,10],with_paths=paths)
def hits(idx,term,path=False):
    return sorted(idx.path_of(n) for n in idx.search(term,limit=50,in_path=path))

idx = index()
idx.apply_usn([change(10,5,'old.txt')])
record('USN-SAME-FRN-IDEMPOTENCY',['/synthetic/docs/old.txt'],hits(idx,'old.txt'))
idx = index()
idx.apply_usn([change(10,5,'old.txt',RENAME_OLD)])
idx.apply_usn([change(10,5,'new.txt',RENAME_NEW)])
record('USN-RENAME-CROSS-BATCH',['/synthetic/docs/new.txt'],hits(idx,'.txt'))
idx = from_entries([('/synthetic/base',True,False),('/synthetic/base/docs',True,False),('/synthetic/base/docs/child.txt',False,False)],'/synthetic/',frns=[1,5,10])
idx.apply_usn([change(5,1,'docs',RENAME_OLD,0x10),change(5,1,'renamed',RENAME_NEW,0x10)])
record('USN-DIRECTORY-RENAME-CHILD',['/synthetic/base/renamed/child.txt'],hits(idx,'child.txt'))
idx = index(True)
idx.apply_usn([change(11,5,'new.txt')])
record('USN-APPEND-PATH-SEARCH',['/synthetic/docs','/synthetic/docs/new.txt','/synthetic/docs/old.txt'],hits(idx,'/synthetic/docs',True))
idx = index()
idx.apply_usn([change(5,0xFFFFFFFF,'docs',DELETE,0x10)])
record('USN-DIRECTORY-DELETE-CHILD',[],hits(idx,'old.txt'))

for label,extensions,expected in [('all',None,True),('none',[],False),('normalized',['.TXT','pdf','txt'],True)]:
    policy = normalize_collection_policy({'extensions':extensions})
    record('POLICY-'+label,expected,accepts(policy,{'is_dir':False,'ext_fold':'txt'}),'contract')
    record('POLICY-'+label+'-directory',True,accepts(policy,{'is_dir':True,'ext_fold':''}),'contract')
record('POLICY-EXT-NORMALIZATION',['pdf','txt'],normalize_collection_policy({'extensions':['.TXT','pdf','txt']})['extensions'],'contract')
for label,value in [('null',None),('boolean-revision',{'revision':True}),('wildcard',{'extensions':['*']}),('unknown-field',{'private':True})]:
    try:
        normalize_collection_policy(value)
        rejected=False
    except CollectionPolicyError:
        rejected=True
    record('POLICY-REJECT-'+label,True,rejected,'contract')
record('QUERY-DIACRITICS-CASE',True,match_text('cafe','CAFÉ report.txt'),'contract')
con = sqlite3.connect(':memory:')
con.execute('CREATE TABLE files(name TEXT,name_fold TEXT)')
con.executemany('INSERT INTO files VALUES(?,?)',[(s,s) for s in ['alpha','beta gamma','alpha gamma','gamma']])
for term,expected in [('alpha|beta gamma',['alpha gamma','beta gamma']),('< >',['alpha','alpha gamma','beta gamma','gamma']),('alpha|',['alpha','alpha gamma','beta gamma','gamma']),('<alpha|beta gamma',['alpha gamma','beta gamma'])]:
    query=compile_query(term)
    actual=sorted(row[0] for row in con.execute('SELECT name FROM files WHERE '+query.sql,query.params))
    record('QUERY-'+term,expected,actual,'contract')
record('QUERY-UNKNOWN-PREFIX-LITERAL',['foo:bar'],compile_query('foo:bar').params,'contract')
for label,extra,expected in [('valid',{},False),('bool-count',{'count':True},True),('relative-root',{'roots':['relative']},True),('nan-elapsed',{'elapsed':float('nan')},True)]:
    value={'version':1,'mode':'local-mounts','state':'paused','roots':['/synthetic/']}
    value.update(extra)
    try:
        normalize_auto_index_state(value)
        rejected=False
    except ValueError:
        rejected=True
    record('SETTINGS-'+label,expected,rejected,'contract')

report={'scope':'Linux synthetic in-memory only; no filesystem scan or real Windows USN journal','results':results,'counts':{'defects_reproduced':sum(r['kind']=='defect' and not r['matches_contract'] for r in results),'contracts_passed':sum(r['kind']=='contract' and r['matches_contract'] for r in results),'contracts_failed':sum(r['kind']=='contract' and not r['matches_contract'] for r in results)}}
print(json.dumps(report,ensure_ascii=False,indent=2,allow_nan=False))
