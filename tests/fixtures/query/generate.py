import importlib.util
import json
import os
from pathlib import Path
import re
import sys
import time
from datetime import datetime, timedelta, timezone

spec = importlib.util.spec_from_file_location('reference', sys.argv[1])
m = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = m
spec.loader.exec_module(m)
os.environ['TZ'] = 'UTC'
time.tzset()
base = Path(__file__).parent
output = {}
for agent in ['claude', 'codex', 'cursor', 'droid', 'opencode', 'pi']:
    golden = json.loads((base.parent / 'jsonl' / f'{agent}.golden.json').read_text())
    entries = []
    for e in golden:
        stamp = e['timestamp']
        if isinstance(stamp, int):
            stamp = m.iso_from_millis(stamp)
        entries.append(m.Entry(e['role'], e['text'], stamp, e['kind'], automated=agent == 'codex' or e['text'] == 'Automated reminder'))
    title = None if agent == 'codex' else {'cursor':'Cursor title', 'opencode':'OpenCode title'}.get(agent, 'Native title')
    meta = m.SessionMeta(agent, 'fixture', 'fixture', title=title, cwd='/fixture/project')
    options = m.SearchOptions('/fixture/project', 'all', None, None, 8, 3)
    searches = {}
    for query in ['needle', 'café', 'Native', 'title']:
        hit = m.scan_entries(m.SessionMeta(agent, 'fixture', 'fixture', title=title, cwd='/fixture/project'), entries, re.compile(re.escape(query), re.I), options)
        searches[query] = None if hit is None else {'score':hit.score, 'matches':hit.matches, 'by_kind':hit.by_kind, 'snippets':hit.snippets}
    reads = {mode: m.transcript_payload(meta, entries, mode, 0, 100000)['body'] for mode in m.MODE_KINDS}
    spans = m.activity_spans(m.session_events(entries, datetime(2026,8,10,tzinfo=timezone.utc), datetime(2026,8,11,tzinfo=timezone.utc)), timedelta(minutes=30))
    output[agent] = {'search':searches, 'read':reads, 'spans':[{'start':int(s.start.timestamp()*1000), 'end':int(s.end.timestamp()*1000), 'human_prompts':s.prompts} for s in spans], 'daily':{str(d):int(t.total_seconds()*1000) for d,t in m.daily_active(spans).items()}}
(base / 'six.golden.json').write_text(json.dumps(output, ensure_ascii=False, indent=2)+'\n')
