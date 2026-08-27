使用Rust语言，Pingora框架，实现一个LLM大模型路由中转服务的系统

配置以及系统模型：

Provider：供应商
  id：供应商id
  key：供应商关键字
  name：供应商名字
  endpont：后端地址
  weight：后端权重

ProviderModel： 供应商提供的模型
  id：关联模型ID
  key：模型的英文关键字
  name：模型的名称
  provider：关联的供应商
  status：模型状态 1在线 0手动离线 -1无法访问导致离线

ProviderKey：供应商的api-key
  id：api-key id
  api_key：api-key
  provider：关联的供应商

Tenant：租户
  id： 租户编号
  name： 租户名称
  domain：关联域名
  cert_key：证书密钥
  cert_file：证书文件
  enableed：是否启用

TenantProvider：租户可以访问的供应商
  id：关联id
  tenant：关联的租户
  provider：关联的供应商

TenantModel：租户可以访问的模型
  id：关联id
  tenant：关联的租户
  model：关联的模型Key

LimitRole：访问限制
  matching_key： 匹配的api-key（留空表示匹配所有api-key）
  matching_model：匹配model（留空表示匹配所有的模型）
  matching_tenant：匹配租户（留空表示匹配所有的租户）
  matching_provider：匹配供应商（留空表示匹配所有供应商）
  limit_count：限额请求书
  limit_token：限额token
  window：限额窗口（m分钟，h小时，d天）


配置默认纯到数据库，可以先看看 Turso (libSQL) 在本地存储在效率上行不行

数据库的配置结构需要映射到内存的数据结构，方便快速查询

系统架构图如下

/*
                 ┌───────────────────────────────────────────────────────────────────┐                     
 ┌────────┐      │  ┌──────────┐              ┌──────────┐        ┌──────────────┐   │    ┌───────────────┐
 │        │      │  │          │   upstream   │          │        │              │   │    │               │
 │ Agent  ┼──────│──►  Pingora ┼──────────────►  Router  ┼────────►  Http Client ┼───│────►  Media Model  │
 │        │      │  │          │              │          │        │              │   │    │               │
 └────────┘      │  └──────────┘              └▲───┬─────┘        └──────────────┘   │    └───────────────┘
                 │                             │   │                                 │                     
                 │  ┌────────────┐             │   │                                 │                     
                 │  │  Web API   ┼─────────────┘   │                                 │                     
Maitaner         │  └─────▲──────┘                 │                                 │                     
                 │        │                  ┌─────▼───────┐                         │    ┌──────────────┐ 
   ┌─┐           │  ┌─────┼──────┐           │             │                         │    │              │ 
   └x┘  ─────────┼──►  Web Admin │           │  SSE Client ┼─────────────────────────│────►   LLM Model  │ 
 xxxxxx          │  └────────────┘           │             │                         │    │              │ 
   xx            │                           └─────────────┘                         │    └──────────────┘ 
 xxxxxx          │                                                                   │                     
 x    x          │                                                      Hydra Server │                     
                 └───────────────────────────────────────────────────────────────────┘                     
 */


Web API提供了一个 Restful的管理接口，可以对所有的配置数据进行增删改查操作



系统的工作过程

1. 启动： 一次性读取数据库所有的配置内容加载到内存数据结构后，启动Pingora
2. Agent发出请求：
   pingora直接将请求upstream转发到 Router -> Router从请求中获取到 域名、Path、api-key、model_name -> 执行路由逻辑找到具体供应商和执行Client -> Spwan Tokio线程，用指定的执行Client去请求实际的模型接口 -> 收到流式的返回就从收到开始就向上级回写, 不是流式就等收完再回写 -> 在最后解析出用量，向clickhouse写入用量信息
3. 路由逻辑：
   3.1 正常逻辑
   通过模型名model_name，从 ProviderModel用key来过滤，找到匹配的供应商列表
   通过域名 从Tenant的domain匹配出租户（匹配不到就直接返回报错）
   如果没有获取到域名，或者获取到localhost，就用localhost去匹配租户（匹配不到就直接返回报错）

   通过匹配到的租户从 TenantProvider 获取租户的供应商列表
   将第一个供应商列表和租户的供应商列表取交集，得到最终的供应商列表，如果交集为空，返回报错

   用最终的供应商列表，对供应商的weight权重做带权重的Round Robin算法，取出一个供应商
   最后通过 供应商查 ProviderKey 得到 api-key列表。
   最后随机取一个api-key
 
   将 请求中的 api-key 替换成前面随机取出来的 api-key，请求的url将 /v1 前面的部分替换成供应商的endpint

   3.2 故障转移
   当请求后端失败后，立即从可用的供应商列表取下一个，然后继续请求，一直到最后没有可用供应商的时候，才最后报错



