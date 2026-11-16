package app.rustsync

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.content.ContextCompat

/** Keeps the Rust daemon alive: other devices connect to this phone while it runs. */
class SyncService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    @Volatile private var running = false
    private var watcher: Thread? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var wakeLock: PowerManager.WakeLock? = null

    /**
     * While files are being transferred, Android must not put Wi-Fi into power saving or let the
     * processor sleep (with the screen off that makes transfers many times slower). Both are held
     * only while something is moving, and let go a little after it stopped.
     */
    private fun watchTransfers(dir: String) {
        if (watcher != null) return
        running = true
        watcher = Thread {
            val control = Control(dir)
            var quietSince = 0L
            while (running) {
                try {
                    Thread.sleep(5_000)
                    val s = control.status()
                    val moving = s.devices.any { it.upRate > 32_000 || it.downRate > 32_000 } ||
                        s.libraries.any { lib -> lib.sync.any { it.online && it.busy() && it.paused.isEmpty() } }
                    val now = System.currentTimeMillis()
                    if (moving) {
                        quietSince = 0L
                        holdLocks(true)
                    } else {
                        if (quietSince == 0L) quietSince = now
                        if (now - quietSince > 30_000) holdLocks(false)
                    }
                } catch (_: InterruptedException) {
                    break
                } catch (_: Exception) {
                    // the daemon is starting or stopping: try again
                }
            }
            holdLocks(false)
        }.also { it.isDaemon = true; it.start() }
    }

    @Suppress("DEPRECATION")
    @Synchronized
    private fun holdLocks(on: Boolean) {
        try {
            if (on) {
                if (wifiLock == null) {
                    val wifi = applicationContext.getSystemService(WifiManager::class.java)
                    // Full speed (no power saving) while the screen is off; low latency while it is on.
                    val mode = if (Build.VERSION.SDK_INT >= 29) WifiManager.WIFI_MODE_FULL_LOW_LATENCY else WifiManager.WIFI_MODE_FULL_HIGH_PERF
                    wifiLock = wifi?.createWifiLock(mode, "rustsync:transfer")?.apply {
                        setReferenceCounted(false)
                        acquire()
                    }
                    if (Build.VERSION.SDK_INT >= 29) {
                        // the low-latency lock only works with the screen on; this one covers the rest
                        wifi?.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "rustsync:transfer-off")?.apply {
                            setReferenceCounted(false)
                            acquire()
                            extraWifiLock = this
                        }
                    }
                }
                if (wakeLock == null) {
                    val power = getSystemService(PowerManager::class.java)
                    wakeLock = power?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "rustsync:transfer")?.apply {
                        setReferenceCounted(false)
                    }
                }
                // renewed every few seconds while transferring; runs out by itself if we hang
                wakeLock?.acquire(60_000)
            } else {
                wifiLock?.let { if (it.isHeld) it.release() }
                wifiLock = null
                extraWifiLock?.let { if (it.isHeld) it.release() }
                extraWifiLock = null
                wakeLock?.let { if (it.isHeld) it.release() }
            }
        } catch (e: Exception) {
            Log.w("rustsync", "cannot change the Wi-Fi or wake lock: ${e.message}")
        }
    }

    private var extraWifiLock: WifiManager.WifiLock? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL, "RustSync", NotificationManager.IMPORTANCE_LOW),
        )
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val notification: Notification = Notification.Builder(this, CHANNEL)
            .setContentTitle("RustSync is running")
            .setContentText("Keeping your libraries in sync")
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setContentIntent(open)
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(1, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(1, notification)
        }
        val dir = Paths.dataDir(this)
        Thread {
            val problem = Core.start(dir, Paths.deviceName(), 3032)
            if (problem.isNotEmpty()) Log.e("rustsync", "cannot start: $problem")
        }.start()
        watchTransfers(dir)
        return START_STICKY
    }

    override fun onDestroy() {
        running = false
        watcher?.interrupt()
        watcher = null
        holdLocks(false)
        Core.stop(Paths.dataDir(this))
        super.onDestroy()
    }

    companion object {
        private const val CHANNEL = "sync"

        fun start(context: Context) {
            ContextCompat.startForegroundService(context, Intent(context, SyncService::class.java))
        }
    }
}

/** Start again after a reboot (once the app has been opened at least once). */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent?) {
        if (intent?.action == Intent.ACTION_BOOT_COMPLETED) {
            SyncService.start(context)
        }
    }
}
