/* Dependency-free, opt-in authenticated fetch/SSE consumer for nulang-ui-msg/1.
 * No URL tokens, cross-origin redirects, implicit draft destruction or access
 * to the older unprotected /__nulang/sse/<room> broadcast channel.
 */
(function (global) {
  "use strict";
  const MAX_FRAME = 1024 * 1024;
  function requirePath(url) {
    if (typeof url !== "string" || !url.startsWith("/") || url.startsWith("//") ||
        /[?#\\\x00-\x20\x7f]/.test(url)) {
      throw Error("UI stream requires a same-origin absolute path without query credentials");
    }
    return url;
  }
  function serial(revision) {
    if (typeof revision !== "string" || !/^(0|[1-9][0-9]*)$/.test(revision) ||
        revision.length > 20) throw Error("invalid UI revision");
    return BigInt(revision);
  }
  function eventRevision(message) {
    if (!message || message.protocol !== "nulang-ui-msg/1") throw Error("unsupported UI protocol");
    if (message.type === "snapshot") return serial(message.document?.revision);
    if (message.type === "patch") return serial(message.patch?.revision);
    throw Error("unsupported UI message");
  }
  class UiStreamClient {
    constructor(host, options) {
      if (!host || typeof host.applyMessage !== "function") throw Error("missing UI DOM host");
      this.host=host;
      this.url=requirePath(options?.url);
      this.fetch=options.fetch || global.fetch.bind(global);
      this.getHeaders=options.getHeaders || (()=>({}));
      this.onRecoveryNeeded=options.onRecoveryNeeded || (()=>{});
      this.onUnauthorized=options.onUnauthorized || (()=>{});
      this.onStatus=options.onStatus || (()=>{});
      this.onError=options.onError || (()=>{});
      this.lastEventId=null;
      this.recovering=false;
      this.stopped=false;
      this.activeController=null;
      this.loop=null;
      this.maxFrameBytes=MAX_FRAME;
    }
    stop() {
      this.stopped=true;
      this.activeController?.abort();
    }
    async connectOnce() {
      if (this.stopped) return "stopped";
      const controller=new AbortController();
      this.activeController=controller;
      try {
        const supplied=await this.getHeaders();
        if (!supplied || typeof supplied !== "object" || Array.isArray(supplied)) {
          throw Error("invalid authorization headers");
        }
        const headers={...supplied,Accept:"text/event-stream"};
        if (!this.recovering && this.lastEventId!==null) headers["Last-Event-ID"]=this.lastEventId;
        else delete headers["Last-Event-ID"];
        const response=await this.fetch(this.url,{
          method:"GET",headers,credentials:"same-origin",cache:"no-store",
          redirect:"error",signal:controller.signal
        });
        if (response.status===401 || response.status===403) {
          this.stopped=true;
          this.onUnauthorized(response.status);
          return "unauthorized";
        }
        if (!response.ok || !response.body || typeof response.body.getReader!=="function") {
          throw Error("UI stream response unavailable");
        }
        if (response.headers?.get && !/^text\/event-stream(?:\s*;|\s*$)/i.test(response.headers.get("content-type")||"")) {
          throw Error("invalid UI stream content-type");
        }
        const reader=response.body.getReader();
        const decoder=new TextDecoder("utf-8",{fatal:true});
        let pending="";
        let sawSnapshot=false;
        while (!this.stopped) {
          const {done,value}=await reader.read();
          if (done) break;
          pending+=decoder.decode(value,{stream:true});
          if (pending.length>this.maxFrameBytes) throw Error("UI SSE frame exceeds budget");
          // The SSE specification permits CRLF. Frames are newline separated.
          pending=pending.replace(/\r\n/g,"\n");
          let boundary;
          while ((boundary=pending.indexOf("\n\n"))!==-1) {
            const frame=pending.slice(0,boundary);
            pending=pending.slice(boundary+2);
            if (!frame || frame.split("\n").every(line=>line.startsWith(":"))) continue;
            const fields={event:null,id:null,data:[]};
            for (const line of frame.split("\n")) {
              if (line.startsWith(":")) continue;
              const colon=line.indexOf(":");
              if (colon===-1) throw Error("invalid UI SSE field");
              const key=line.slice(0,colon);
              let body=line.slice(colon+1);
              if (body.startsWith(" ")) body=body.slice(1);
              if (key==="event") fields.event=body;
              else if (key==="id") fields.id=body;
              else if (key==="data") fields.data.push(body);
            }
            if (fields.event!=="nulang-ui") continue; // heartbeat/comment only
            const message=JSON.parse(fields.data.join("\n"));
            const wireRevision=eventRevision(message);
            if (fields.id===null || serial(fields.id)!==wireRevision) {
              throw Error("UI event ID and revision mismatch");
            }
            if (message.type==="patch" && !sawSnapshot) {
              this.recovering=true;
              this.lastEventId=null;
              this.onRecoveryNeeded("authoritative snapshot required before patches");
              await reader.cancel();
              return "recover";
            }
            if (message.type==="patch" &&
                serial(message.patch?.base_revision)!==serial(this.host.document.revision)) {
              this.recovering=true;
              this.lastEventId=null;
              this.onRecoveryNeeded("revision gap: authoritative snapshot required");
              await reader.cancel();
              return "recover";
            }
            // A reconnect's snapshot may hold newer server state. Never
            // silently overwrite unacknowledged local input. The host rejects
            // resync until the application explicitly resolves those drafts.
            try {
              this.host.applyMessage(message);
            } catch (error) {
              this.recovering=true;
              this.lastEventId=null;
              this.onRecoveryNeeded(String(error.message||error));
              await reader.cancel();
              return "recover";
            }
            if (message.type==="snapshot") sawSnapshot=true;
            this.lastEventId=fields.id;
            this.recovering=false;
          }
        }
        // Each connection should start with a server-owned snapshot. A bare
        // patch-only stream is never used to bridge revisions after reconnect.
        if (!sawSnapshot && !this.stopped) {
          this.recovering=true;
          this.onRecoveryNeeded("missing authoritative snapshot");
          return "recover";
        }
        return this.stopped?"stopped":"closed";
      } finally {
        if (this.activeController===controller) this.activeController=null;
      }
    }
    start() {
      if (this.loop) return this.loop;
      this.stopped=false;
      this.loop=(async()=>{
        let retry=0;
        while (!this.stopped) {
          let result;
          try {
            result=await this.connectOnce();
          } catch (error) {
            if (this.stopped) break;
            this.onError(error);
            result="error";
          }
          this.onStatus(result);
          if (this.stopped || result==="unauthorized" || result==="stopped") break;
          // Missing revisions force a new authenticated snapshot on next fetch.
          // Backoff keeps the browser from spinning on a denied or faulty host.
          retry=Math.min(retry+1,5);
          await new Promise(resolve=>setTimeout(resolve,Math.min(250*2**(retry-1),4000)));
        }
        this.loop=null;
      })();
      return this.loop;
    }
  }
  global.nulangUiStream=Object.freeze({
    createClient:(host,options)=>new UiStreamClient(host,options)
  });
})(globalThis);
