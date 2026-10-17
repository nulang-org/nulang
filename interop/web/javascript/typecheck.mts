import {NulangWebClient, type OpenApiContract, ApiError} from './client.mjs';
const contract:OpenApiContract={openapi:'3.1.0',paths:{}};
const client=new NulangWebClient({baseUrl:'https://example.com',contract});
async function main(){
  const result=await client.call('my_operation',{path:{id:'hello'},query:{limit:5,tags:['a','b']}});
  const text:string=await result.text();
  if(result.status===400) throw new ApiError(400,text,result.headers);
}
void main;
