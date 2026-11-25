package io.github.rusttalkcrew.rusttalk;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.util.Log;

/**
 * Keeps the microphone and the connection alive while a call runs in the background
 * or with the screen locked. Without a microphone foreground service Android silences
 * the recording once the app leaves the screen and freezes the cached process.
 * Started and stopped by MainActivity.communicationMode.
 */
public class CallService extends Service {
    private static final String CHANNEL = "calls";
    private static final int ID = 1;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        NotificationManager manager = getSystemService(NotificationManager.class);
        manager.createNotificationChannel(new NotificationChannel(CHANNEL,
                getString(R.string.call_channel), NotificationManager.IMPORTANCE_LOW));
        // Like the launcher: brings the running activity back instead of starting another.
        Intent open = new Intent(this, MainActivity.class)
                .setAction(Intent.ACTION_MAIN)
                .addCategory(Intent.CATEGORY_LAUNCHER)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED);
        Notification.Builder notification = new Notification.Builder(this, CHANNEL)
                .setSmallIcon(R.drawable.ic_call)
                .setContentTitle(getString(R.string.call_title))
                .setContentText(getString(R.string.call_text))
                .setCategory(Notification.CATEGORY_CALL)
                .setOngoing(true)
                .setContentIntent(PendingIntent.getActivity(this, 0, open,
                        PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT));
        if (Build.VERSION.SDK_INT >= 31)
            notification.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE);
        try {
            if (Build.VERSION.SDK_INT >= 29)
                startForeground(ID, notification.build(), ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE);
            else
                startForeground(ID, notification.build());
        } catch (IllegalStateException | SecurityException error) {
            // ForegroundServiceStartNotAllowedException (Android 12+, an IllegalStateException)
            // when the app isn't in the foreground; SecurityException without microphone
            // access (Android 14+). The call goes on while the app stays on screen.
            Log.w("RustTalk", "Call foreground service not allowed", error);
            stopSelf();
        }
        // A restarted process has no call to keep alive.
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
