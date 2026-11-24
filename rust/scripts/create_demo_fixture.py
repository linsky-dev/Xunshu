#!/usr/bin/env python3
"""Development fixture only. The Rust application never invokes Python."""
import argparse
from pathlib import Path
p=argparse.ArgumentParser()
p.add_argument('--destination',required=True,type=Path)
a=p.parse_args()
root=a.destination
if not root.is_absolute():p.error('destination must be absolute')
root.mkdir(parents=True,exist_ok=True)
files={
 '人事资料/干部任免审批表.docx':'Synthetic filename-index fixture. This is not a real Word document.',
 '人事资料/2026年度考核汇总.pdf':'Synthetic filename-index fixture. This is not a real PDF.',
 '项目预算_2026.xlsx':'Synthetic filename-index fixture.',
 '寻枢设计方案.md':'# 合成样本\n本地文件搜索测试，无真实个人信息。',
 '会议纪要_1108.txt':'这是一份合成测试样本，不作业务用途。',
 'readme_source.rs':'fn main() {}',
 '预算说明.txt':'预算数据为合成测试，不作业务用途。',
}
for relative,text in files.items():
 target=root/relative;target.parent.mkdir(parents=True,exist_ok=True)
 with target.open('x',encoding='utf-8') as stream:stream.write(text)
print(f'Created {len(files)} synthetic files in {root}')
