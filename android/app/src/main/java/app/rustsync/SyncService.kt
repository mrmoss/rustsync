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
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.content.ContextCompat

/** Keeps the Rust daemon alive: other devices connect to this phone while it runs. */
class SyncService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

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
        return START_STICKY
    }

    override fun onDestroy() {
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
