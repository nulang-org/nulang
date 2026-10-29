// Integrated race: canonical GET after POST must not overwrite a newer SSE patch.
// Run: node scripts/tests/web_action_sse_race.mjs
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';
const source=readFileSync(new URL('../../src/web/reactivity.rs',import.meta.url),'utf8');
const match=source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(match);
const nodes=new Map();
const makeNode=(id,text)=>({id,textContent:text,replaceWith(n){nodes.set(id,makeNode(id,n.textContent));}});
nodes.set('detail',makeNode('detail','Old detail'));
nodes.set('summary',makeNode('summary','Old summary'));
const actions=new Map(), requests=[], errors=[], instances=[];
const form={
  tagName:'FORM',
  dataset:{action:'save',actionPlacement:'server',nulangTarget:'#detail',nulangRevalidate:'#summary'},
  closest(which){return which==='form'?this:null;},
  addEventListener(name,cb){actions.set(name,cb);}
};
const document={
  readyState:'complete',
  querySelectorAll(selector){return selector==='[data-action]'?[form]:[];},
  querySelector(selector){return nodes.get(selector.slice(1))||null;},
  importNode(n){return makeNode(n.id,n.textContent);}
};
class DOMParser {
  parseFromString(html){
    return {querySelector(selector){
      const id=selector.slice(1);
      const m=html.match(new RegExp('<div id="'+id+'">([^<]*)</div>'));
      return m?makeNode(id,m[1]):null;
    }};
  }
}
class EventSource {
  constructor(){this.handlers=new Map();this.closed=false;instances.push(this);}
  addEventListener(name,cb){this.handlers.set(name,cb);}
  emit(name,payload){this.handlers.get(name)?.({data:JSON.stringify(payload)});}
  close(){this.closed=true;}
}
class FormData {forEach(){}}
const html=(body)=>({ok:true,status:200,redirected:false,
  headers:{get:()=> 'text/html'}, text:async()=>body});
const fetch=(url,options)=>{
  requests.push({url,options});
  if(options.method==='POST') return Promise.resolve(html('<div id="detail">Old POST detail</div>'));
  return new Promise(resolve=>{requests[requests.length-1].resolve=resolve;});
};
const window={EventSource,location:{href:'https://local.test/tasks'}};
runInNewContext(match[1],{window,document,DOMParser,FormData,URLSearchParams,AbortController,fetch,
  console:{error(...args){errors.push(args.map(String).join(' '));},warn(){}}});
const sub=window.nulang.stream('tasks');
const protocol='nulang-ui-html/1';
instances[0].emit('nulang-ui-snapshot',
  {type:'snapshot',protocol,document_id:'tasks',revision:'1',elements:[]});
actions.get('submit')({preventDefault(){}});
for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
assert.deepEqual(requests.map(r=>r.options.method),['POST','GET']);
instances[0].emit('nulang-ui-patch',{
  type:'patch',protocol,document_id:'tasks',base_revision:'1',revision:'2',
  element:{target:'#summary',html:'<div id="summary">New SSE summary</div>'}
});
assert.equal(nodes.get('summary').textContent,'New SSE summary');
requests[1].resolve(html('<div id="summary">Stale GET summary</div>'));
for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
assert.equal(nodes.get('summary').textContent,'New SSE summary',
  'later SSE update cannot be rolled back by earlier canonical GET');
assert.equal(nodes.get('detail').textContent,'Old detail',
  'stale multi-region response should not partly patch the primary');
assert.equal(errors.length,0);
sub.close();
console.log('Nulang Web: cross-feature SSE/action canonical GET race protected');
