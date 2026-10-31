package app.rustsync

import android.content.Context
import android.os.Build
import java.io.File

/** The Rust core (the same code the desktop program runs), loaded from librustsync.so. */
object Core {
    init {
        System.loadLibrary("rustsync")
    }

    /** Start the daemon. Returns "" or what went wrong. */
    @JvmStatic external fun start(dataDir: String, name: String, port: Int): String

    @JvmStatic external fun stop(dataDir: String)

    @JvmStatic external fun socketPath(dataDir: String): String

    @JvmStatic external fun version(): String
}

object Paths {
    /** Where the daemon keeps its keys, paired devices and indexes (private to the app). */
    fun dataDir(context: Context): String = File(context.filesDir, "rustsync").absolutePath

    fun deviceName(): String {
        val model = Build.MODEL?.trim().orEmpty()
        return if (model.isEmpty()) "android" else model
    }
}
