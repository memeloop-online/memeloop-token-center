(module
  (type $log (func (param i32 i32 i32 i32)))
  (type $kv-get (func (param i32 i32 i32)))
  (type $kv-put (func (param i32 i32 i32 i32 i32)))
  (type $http-request (func (param i32 i32 i32 i32 i32 i32 i32 i32 i32)))
  (type $context-call (func
    (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)
    (result i32)))
  (type $post-one (func (param i32)))
  (type $string-call (func (param i32 i32) (result i32)))
  (type $realloc (func (param i32 i32 i32 i32) (result i32)))
  (type $initialize (func))

  (import "cm32p2|memeloop:token-center/host@0.1" "log" (func $log (type $log)))
  (import "cm32p2|memeloop:token-center/host@0.1" "kv-get" (func $kv-get (type $kv-get)))
  (import "cm32p2|memeloop:token-center/host@0.1" "kv-put" (func $kv-put (type $kv-put)))
  (import "cm32p2|memeloop:token-center/host@0.1" "http-request" (func $http-request (type $http-request)))

  (memory $memory 1)
  (global $heap (mut i32) (i32.const 8192))
  (data (i32.const 64) "{\22model\22:\22example-rewritten\22,\22messages\22:[{\22role\22:\22user\22,\22content\22:\22rewritten by plugin\22}]}")
  (data (i32.const 192) "[\22example-rewritten\22]")
  (data (i32.const 224) "USD")
  (data (i32.const 232) "0.001")
  (data (i32.const 400) "example-rewritten")

  (func $post-auth (type $context-call)
    (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)
    (result i32)
    ;; BEGIN POST-AUTH BODY
    ;; result<decision,string>: Ok, allow, replace model and request JSON.
    i32.const 256 i32.const 0 i32.store
    i32.const 260 i32.const 1 i32.store
    i32.const 264 i32.const 0 i32.store
    i32.const 276 i32.const 1 i32.store
    i32.const 280 i32.const 400 i32.store
    i32.const 284 i32.const 17 i32.store
    i32.const 288 i32.const 0 i32.store
    i32.const 300 i32.const 1 i32.store
    i32.const 304 i32.const 64 i32.store
    i32.const 308 i32.const 90 i32.store
    i32.const 256
    ;; END POST-AUTH BODY
  )

  (func $post-auth-post (type $post-one) (param i32))

  (func $list-models (type $string-call) (param i32 i32) (result i32)
    i32.const 320 i32.const 0 i32.store
    i32.const 324 i32.const 192 i32.store
    i32.const 328 i32.const 21 i32.store
    i32.const 320)

  (func $list-models-post (type $post-one) (param i32))

  (func $quote (type $context-call)
    (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)
    (result i32)
    i32.const 336 i32.const 0 i32.store
    i32.const 344 i32.const 224 i32.store
    i32.const 348 i32.const 3 i32.store
    i32.const 352 i32.const 232 i32.store
    i32.const 356 i32.const 5 i32.store
    i32.const 360 i64.const 1 i64.store
    i32.const 368 i64.const 1 i64.store
    i32.const 376 i32.const 1 i32.store
    i32.const 336)

  (func $quote-post (type $post-one) (param i32))

  (func $realloc (type $realloc) (param i32 i32 i32 i32) (result i32)
    (local $result i32)
    global.get $heap
    local.tee $result
    local.get 3
    i32.add
    global.set $heap
    local.get $result)

  (func $initialize (type $initialize))

  (export "cm32p2|memeloop:token-center/traffic-policy@0.1|post-auth" (func $post-auth))
  (export "cm32p2|memeloop:token-center/traffic-policy@0.1|post-auth_post" (func $post-auth-post))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.1|list-models" (func $list-models))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.1|list-models_post" (func $list-models-post))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.1|quote" (func $quote))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.1|quote_post" (func $quote-post))
  (export "cm32p2_memory" (memory $memory))
  (export "cm32p2_realloc" (func $realloc))
  (export "cm32p2_initialize" (func $initialize))
)
