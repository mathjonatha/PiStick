#!/bin/sh
set -eu
aqui=$(cd "$(dirname "$0")" && pwd)
raiz=$(cd "$aqui/../.." && pwd)
rea="/c/Users/math/Documents/02. REA"
jdk="$rea/jdk/jdk-21.0.12.1+1/bin"
sdk="$rea/android-sdk"
bt="$sdk/build-tools/36.0.0"
jar="$sdk/platforms/android-36/android.jar"
saida="${1:-$raiz/tools/pi/android/tom.apk}"
obj="$raiz/.cache/android-tom"
rm -rf "$obj"
mkdir -p "$obj/classes" "$obj/dex"

"$jdk/javac.exe" --release 8 -Xlint:-options -cp "$jar" -d "$obj/classes" "$aqui/src/br/pistick/tom/MainActivity.java"
"$jdk/java.exe" -cp "$bt/lib/d8.jar" com.android.tools.r8.D8 --lib "$jar" --min-api 26 --output "$obj/dex" "$obj/classes/br/pistick/tom/"*.class
"$bt/aapt2.exe" link -o "$obj/base.apk" -I "$jar" --manifest "$aqui/AndroidManifest.xml" --min-sdk-version 26 --target-sdk-version 33
(cd "$obj/dex" && "$jdk/jar.exe" uf "$obj/base.apk" classes.dex)
"$bt/zipalign.exe" -f -p 4 "$obj/base.apk" "$obj/alinhado.apk"
chave="$raiz/.cache/chave-depuracao.jks"
if [ ! -f "$chave" ]; then
    "$jdk/keytool.exe" -genkeypair -keystore "$chave" -storepass android -keypass android -alias depuracao \
        -dname "CN=PiStick depuracao" -keyalg RSA -keysize 2048 -validity 10000
fi
"$jdk/java.exe" -jar "$bt/lib/apksigner.jar" sign --v4-signing-enabled false --ks "$chave" --ks-pass pass:android --key-pass pass:android --out "$saida" "$obj/alinhado.apk"
"$jdk/java.exe" -jar "$bt/lib/apksigner.jar" verify "$saida"
ls -l "$saida"
