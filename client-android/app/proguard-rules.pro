# Keep JNI bridge methods called from libwaystone_rtc.so.
-keep class dev.waystone.app.stream.Transport {
    native <methods>;
}
-keep interface dev.waystone.app.stream.Transport$NativeCallback {
    public void onNativeEvent(int, byte[]);
}
-keep class * implements dev.waystone.app.stream.Transport$NativeCallback {
    public void onNativeEvent(int, byte[]);
}
