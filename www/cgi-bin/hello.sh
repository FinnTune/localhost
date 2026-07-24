#!/bin/sh
echo "Content-Type: text/plain"
echo ""
echo "Hello from CGI!"
echo "Method: $REQUEST_METHOD"
echo "Query: $QUERY_STRING"
echo "Server: $SERVER_NAME:$SERVER_PORT"
echo "Remote: $REMOTE_ADDR"
if [ "$REQUEST_METHOD" = "POST" ]; then
  echo "Body:"
  cat
fi
