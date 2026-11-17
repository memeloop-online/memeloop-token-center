import { createHash } from "node:crypto";
export class FixtureVerifier {
  private carry="";private payloadBytes=0;private terminal=false;private valid=true;private hash=createHash("sha256");
  constructor(private protocol:"chat"|"responses",private stream:boolean,private fixtureBytes:number){}
  private content(text:unknown){if(typeof text!=="string"||/[^x]/.test(text)){this.valid=false;return;}this.payloadBytes+=Buffer.byteLength(text);this.hash.update(text);}
  consume(chunk:Uint8Array){this.carry+=Buffer.from(chunk).toString("utf8");if(!this.stream)return;
    let end:number;while((end=this.carry.indexOf("\n\n"))>=0){const frame=this.carry.slice(0,end);this.carry=this.carry.slice(end+2);const data=frame.split("\n").find(line=>line.startsWith("data: "))?.slice(6);if(!data)continue;if(data==="[DONE]"){this.terminal=true;continue;}
      try{if(this.protocol==="responses"){if(frame.startsWith("event: response.completed"))this.terminal=true;else if(frame.startsWith("event: response.output_text.delta"))this.content(JSON.parse(data).delta);}else{const parsed=JSON.parse(data);for(const choice of parsed.choices??[]){if(choice.delta?.content!==undefined)this.content(choice.delta.content);}}}catch{this.valid=false;}}
  }
  finish(wireBytes:number){if(!this.stream){try{const value=JSON.parse(this.carry);this.content(this.protocol==="responses"?value.output[0].content[0].text:value.choices[0].message.content);}catch{this.valid=false;}}
    const wireTarget=this.stream&&this.protocol==="chat";
    return {verified:this.valid&&(!this.stream||this.terminal)&&(wireTarget?wireBytes===this.fixtureBytes:this.payloadBytes===this.fixtureBytes),response_payload_bytes:this.payloadBytes,response_payload_sha256:this.hash.digest("hex"),expected_response_size_kind:wireTarget?"wire":"payload",expected_response_fixture_bytes:this.fixtureBytes};
  }
}
