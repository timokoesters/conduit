#!/bin/sh


cp /ca/ca.crt /usr/local/share/ca-certificates/ca.crt
update-ca-certificates

openssl req -nodes -new -newkey rsa:2048 -keyout /workdir/key.pem -x509 -days 365 -out /workdir/certificate.csr -addext "subjectAltName=DNS:${SERVER_NAME}"
openssl x509 -req -in /workdir/certificate.csr -CA /ca/ca.crt -CAkey /ca/ca.key -CAcreateserial -out /workdir/certificate.crt -days 500 -sha256

temp_file=$(mktemp)
tee $temp_file <<-EOM
listen 8448 ssl http2;
ssl_certificate     /workdir/certificate.crt;
ssl_certificate_key /workdir/key.pem;
location / {
                    proxy_pass http://127.0.0.1:8008;
}
EOM

sed  -i '/listen       80;/r '"$temp_file" /etc/nginx/conf.d/default.conf
sed  -i 'd/listen       80;/' /etc/nginx/conf.d/default.conf

nginx
exec /workdir/conduit