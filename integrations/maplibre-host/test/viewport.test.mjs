import test from 'node:test';
import assert from 'node:assert/strict';
import { observeViewportGeoJson } from '../viewport.mjs';

const empty = { type: 'FeatureCollection', features: [] };
const next = { type: 'FeatureCollection', features: [{ type: 'Feature', id: 1, properties: {}, geometry: {type:'Point', coordinates:[0,0]} }] };
const sleep = () => new Promise(resolve => setTimeout(resolve, 15));
const deferred = () => { let resolve; let reject; const promise = new Promise((res,rej)=>{resolve=res; reject=rej}); return {promise,resolve,reject}; };
function fixture() {
  let bounds = [-48,-16,-47,-15]; let zoom = 8;
  const listeners = new Set(); const commands = [];
  return {
    commands, listeners,
    host: {
      viewport: () => ({bbox:[...bounds],zoom}),
      dispatch: command => commands.push(command),
      on(event,listener) { assert.equal(event,'moveend'); listeners.add(listener); return () => listeners.delete(listener); },
    },
    move(b,z=zoom) {bounds=b;zoom=z; for(const listener of listeners)listener();},
  };
}

test('initial request publishes only valid GeoJSON and moveend refetches latest viewport', async () => {
  const f=fixture(); const seen=[];
  const dispose=observeViewportGeoJson({host:f.host,sourceId:'sites',debounceMs:0,fetchFeatures:async args=>{seen.push(args);return next;}});
  await sleep();
  assert.equal(seen.length,1);
  assert.deepEqual(seen[0].bbox,[-48,-16,-47,-15]);
  assert.deepEqual(f.commands.at(-1).source.data,next);
  f.move([-49,-17,-48,-16],9);
  await sleep();
  assert.equal(seen.length,2);
  assert.equal(seen[1].zoom,9);
  dispose();
  assert.equal(f.listeners.size,0);
});

test('obsolete fetch is aborted immediately and late response cannot overwrite newer viewport', async () => {
  const f=fixture(); const slow=deferred(); let calls=0,signal;
  const dispose=observeViewportGeoJson({host:f.host,sourceId:'sites',debounceMs:0,fetchFeatures:({signal:s})=>{
    calls++; if(calls===1){signal=s;return slow.promise;} return Promise.resolve(next);
  }});
  await sleep();
  f.move([-49,-17,-48,-16]);
  assert.equal(signal.aborted,true);
  await sleep();
  slow.resolve({type:'FeatureCollection',features:[{id:'obsolete'}]});
  await sleep();
  assert.deepEqual(f.commands.at(-1).source.data,next);
  dispose();
});

test('duplicate viewport events are deduplicated, manual refresh is explicit', async () => {
  const f=fixture();let calls=0;
  const dispose=observeViewportGeoJson({host:f.host,sourceId:'sites',debounceMs:0,fetchFeatures:async()=>{calls++;return empty;}});
  await sleep();f.move([-48,-16,-47,-15]); await sleep();
  assert.equal(calls,1);
  dispose.refresh(); await sleep();
  assert.equal(calls,2);
  dispose();
});

test('invalid response reports error and leaves existing source state intact', async () => {
  const f=fixture();const errors=[];
  const dispose=observeViewportGeoJson({host:f.host,sourceId:'sites',debounceMs:0,fetchFeatures:async()=>({features:'bad'}),onError:error=>errors.push(error)});
  await sleep();
  assert.equal(errors.length,1);
  assert.match(errors[0].message,/FeatureCollection/);
  assert.deepEqual(f.commands, []);
  dispose();
});

test('disposing aborts pending work and clears subscribed callbacks', async () => {
  const f=fixture();const request=deferred();let signal;
  const dispose=observeViewportGeoJson({host:f.host,sourceId:'sites',debounceMs:0,fetchFeatures:({signal:s})=>{signal=s;return request.promise;}});
  await sleep();dispose();
  assert.equal(signal.aborted,true);
  assert.equal(f.listeners.size,0);
  request.resolve(next);await sleep();
  assert.deepEqual(f.commands,[]);
});
