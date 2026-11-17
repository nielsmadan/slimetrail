import importlib.util,sys,json
from pathlib import Path
reference=Path(sys.argv[1])
spec=importlib.util.spec_from_file_location('search_agent_logs',reference)
m=importlib.util.module_from_spec(spec);sys.modules[spec.name]=m;spec.loader.exec_module(m)
out=Path(__file__).parent
ts='2026-08-10T12:03:00+02:00'
content=[{'type':'text','text':'Unicode café 🐌\nsecond line  '},{'type':'tool_use','name':'Shell','input':{'command':'echo needle','cwd':'/fixture'}},{'type':'tool_result','content':'result needle'}]
fixtures={
 'claude':[{'type':'user','sessionId':'claude-canonical','uuid':'u1','cwd':'/fixture/project','gitBranch':'feature','timestamp':ts,'message':{'content':content}},{'type':'assistant','timestamp':ts,'message':'Done'},{'type':'custom-title','customTitle':'Native title'},{'type':'user','isMeta':True,'timestamp':ts,'message':{'content':'Automated reminder'}}],
 'codex':[{'type':'session_meta','payload':{'id':'codex-canonical','session_id':'root-thread','cwd':'/fixture/project','git':{'branch':'feature'},'source':{'subagent':{'thread_spawn':{'parent_thread_id':'root-thread'}}}}},{'type':'response_item','timestamp':ts,'payload':{'type':'message','role':'user','content':'Unicode café 🐌\nsecond line  '}},{'type':'response_item','timestamp':ts,'payload':{'type':'function_call','name':'exec','arguments':'{"cmd":"echo needle"}'}},{'type':'response_item','timestamp':ts,'payload':{'type':'function_call_output','output':'needle result'}},{'type':'response_item','timestamp':ts,'payload':{'type':'reasoning','summary':[{'type':'summary_text','text':'Consider paths'}]}},{'type':'turn_context','timestamp':ts,'payload':{'summary':'A summary'}},{'type':'response_item','payload':{'type':'message','role':'developer','content':'hidden preamble'}},{'type':'response_item','payload':{'type':'message','role':'user','content':'# AGENTS.md instructions\ninjected'}}],
 'droid':[{'type':'session_start','id':'droid-canonical','cwd':'/fixture/project','title':'Native title'},{'type':'message','id':'m1','timestamp':ts,'message':{'role':'user','content':content}},{'type':'message','timestamp':ts,'message':{'role':'assistant','content':'Done'}},{'type':'usage','tokens':42}],
 'pi':[{'type':'session','id':'pi-canonical','cwd':'/fixture/project','name':'Native title'},{'type':'message','id':'p1','timestamp':ts,'message':{'role':'user','content':content}},{'type':'message','timestamp':ts,'message':{'role':'assistant','content':'Done'}},{'type':'model_change','modelId':'ignored'}]
}
for agent, records in fixtures.items():
 path=out/f'{agent}.jsonl';path.write_text(''.join(json.dumps(r,ensure_ascii=False)+'\n' for r in records))
 entries=[e for r in records for e in getattr(m,f'{agent}_entries')(r)]
 (out/f'{agent}.golden.json').write_text(json.dumps([{'role':e.role,'kind':e.kind,'text':e.text,'timestamp':e.timestamp} for e in entries],ensure_ascii=False,indent=2)+'\n')
