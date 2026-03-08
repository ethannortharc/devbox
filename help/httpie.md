# HTTPie — HTTP Client

## GET Requests
  http :8080/api/users         GET localhost:8080
  http example.com/api         GET with full URL
  http :8080/api q==search     Query parameter

## POST Requests
  http POST :8080/api name=foo   JSON body
  http POST :8080/api < data.json   From file

## Headers & Auth
  http :8080/api Authorization:Bearer\ token
  http -a user:pass :8080/api    Basic auth

## Common Flags
  -v           Verbose (show request + response)
  -h           Headers only
  -b           Body only
  -d           Download mode
  -f           Form data (not JSON)
  --pretty=all Format and color output

## Examples
  http :8080/api/health
  http POST :8080/api/users name=alice email=a@b.com
  http PUT :8080/api/users/1 name=bob
  http DELETE :8080/api/users/1
  http :8080/api Accept:text/plain
