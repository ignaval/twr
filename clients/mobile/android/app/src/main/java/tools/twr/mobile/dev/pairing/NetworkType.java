package tools.twr.mobile.dev.pairing;

/** The device's default network, as the {@code network} event and the saved {@code last} routes name it. */
public enum NetworkType {
    WIFI("wifi"),
    CELLULAR("cellular"),
    OTHER("other"),
    NONE("none");

    public final String wire;

    NetworkType(String wire) {
        this.wire = wire;
    }

    /**
     * From the default network's transports. Wi-Fi wins over cellular when both are listed; a VPN
     * carries its underlying network's transports, so Tailscale over cellular is cellular.
     */
    public static NetworkType of(boolean connected, boolean wifi, boolean cellular) {
        if (!connected) return NONE;
        if (wifi) return WIFI;
        if (cellular) return CELLULAR;
        return OTHER;
    }

    /** The key {@code last} is saved under: with no network, the route that wins is "other". */
    public String lastKey() {
        return this == NONE ? OTHER.wire : wire;
    }
}
