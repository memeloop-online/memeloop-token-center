(module
  (type $log (func (param i32 i32 i32 i32)))
  (type $kv-get (func (param i32 i32 i32)))
  (type $kv-put (func (param i32 i32 i32 i32 i32)))
  (type $http-request (func (param i32 i32 i32 i32 i32 i32 i32 i32 i32)))
  (type $context-call (func
    (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)
    (result i32)))
  (type $prepare-call (func
    (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)
    (result i32)))
  (type $post-one (func (param i32)))
  (type $string-call (func (param i32 i32) (result i32)))
  (type $realloc (func (param i32 i32 i32 i32) (result i32)))
  (type $initialize (func))

  (import "cm32p2|memeloop:token-center/host@0.2" "log" (func $log (type $log)))
  (import "cm32p2|memeloop:token-center/host@0.2" "kv-get" (func $kv-get (type $kv-get)))
  (import "cm32p2|memeloop:token-center/host@0.2" "kv-put" (func $kv-put (type $kv-put)))
  (import "cm32p2|memeloop:token-center/host@0.2" "http-request" (func $http-request (type $http-request)))

  (memory $memory 1)
  (global $heap (mut i32) (i32.const 8192))
  (data (i32.const 64) "{\22model\22:\22example-rewritten\22,\22messages\22:[{\22role\22:\22user\22,\22content\22:\22rewritten by plugin\22}]}")
  (data (i32.const 192) "[\22example-rewritten\22]")
  (data (i32.const 224) "USD")
  (data (i32.const 232) "0.001")
  (data (i32.const 400) "example-rewritten")
  (data (i32.const 512) "{\22method\22:\22POST\22,\22path\22:\22/vendor/infer\22,\22headers\22:{\22content-type\22:\22application/json\22,\22x-plugin-shape\22:\22buffered-v1\22},\22body_base64\22:\22eyJwcm9tcHQiOiJmcm9tLWNvbXBvbmVudCIsIm1vZGVsIjoidmVuZG9yLW1vZGVsIn0=\22,\22streaming\22:false}")
  (data (i32.const 1024) "{\22status\22:200,\22headers\22:{\22content-type\22:\22application/json\22},\22body_base64\22:\22eyJpZCI6ImNtcC0xIiwib2JqZWN0IjoiY2hhdC5jb21wbGV0aW9uIiwiY2hvaWNlcyI6W3sibWVzc2FnZSI6eyJyb2xlIjoiYXNzaXN0YW50IiwiY29udGVudCI6Im5vcm1hbGl6ZWQgYnkgY29tcG9uZW50In19XSwidXNhZ2UiOnsicHJvbXB0X3Rva2VucyI6NywiY29tcGxldGlvbl90b2tlbnMiOjN9fQ==\22,\22usage\22:{\22input_tokens\22:7,\22output_tokens\22:3,\22estimated\22:false}}")

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

  (func $prepare (type $prepare-call)
    (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)
    (result i32)
    ;; BEGIN PREPARE BODY
    i32.const 768 i32.const 0 i32.store
    i32.const 772 i32.const 512 i32.store
    i32.const 776 i32.const 220 i32.store
    i32.const 768
    ;; END PREPARE BODY
  )

  (func $prepare-post (type $post-one) (param i32))

  (func $normalize (type $context-call)
    (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)
    (result i32)
    ;; BEGIN NORMALIZE BODY
    i32.const 784 i32.const 0 i32.store
    i32.const 788 i32.const 1024 i32.store
    i32.const 792 i32.const 372 i32.store
    i32.const 784
    ;; END NORMALIZE BODY
  )

  (func $normalize-post (type $post-one) (param i32))

  (func $realloc (type $realloc) (param i32 i32 i32 i32) (result i32)
    (local $result i32)
    global.get $heap
    local.tee $result
    local.get 3
    i32.add
    global.set $heap
    local.get $result)

  (func $initialize (type $initialize))

  (export "cm32p2|memeloop:token-center/traffic-policy@0.2|post-auth" (func $post-auth))
  (export "cm32p2|memeloop:token-center/traffic-policy@0.2|post-auth_post" (func $post-auth-post))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.2|list-models" (func $list-models))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.2|list-models_post" (func $list-models-post))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.2|quote" (func $quote))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.2|quote_post" (func $quote-post))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.2|prepare" (func $prepare))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.2|prepare_post" (func $prepare-post))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.2|normalize" (func $normalize))
  (export "cm32p2|memeloop:token-center/upstream-provider@0.2|normalize_post" (func $normalize-post))
  (export "cm32p2_memory" (memory $memory))
  (export "cm32p2_realloc" (func $realloc))
  (export "cm32p2_initialize" (func $initialize))
)
