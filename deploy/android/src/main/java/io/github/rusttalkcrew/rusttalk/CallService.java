package io.github.rusttalkcrew.rusttalk;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Person;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.graphics.drawable.Icon;
import android.os.Build;
import android.os.IBinder;
import android.util.Log;

/**
 * Keeps the microphone and the connection alive while a call runs in the background
 * or with the screen locked. Without a microphone foreground service Android silences
 * the recording once the app leaves the screen and freezes the cached process.
 * Started and stopped by MainActivity.communicationMode; the app sends what the
 * notification shows (SHOW, crates/client/src/android.rs call_notice). On Android 12+
 * the notification is an ongoing call (CallStyle) with Hang up and Mute; before, the
 * plain notification with the same buttons.
 */
public class CallService extends Service {
    private static final String CHANNEL = "calls";
    private static final int ID = 1;
    // The buttons, numbered as Button in crates/client/src/call/notice.rs.
    private static final int MUTE = 1;
    private static final int HANG_UP = 2;
    private static final String PRESS = "io.github.rusttalkcrew.rusttalk.PRESS";
    private static final String SHOW = "io.github.rusttalkcrew.rusttalk.SHOW_CALL";
    private static final String OPEN = "io.github.rusttalkcrew.rusttalk.OPEN_CALL";

    // Kept across service instances: leaving and joining in one frame starts a new one.
    private static String name = "";
    private static String server = "";
    private static boolean muted;
    private static boolean canMute = true;
    private boolean foreground;

    /** Acts at once, also with no window in the background; returns whether the mic is off. */
    private static native boolean press(int button);
    private static native void openCall();

    /** MainActivity.onNewIntent: a tap on the notification shows the call. */
    static void opened(Intent intent) {
        if (OPEN.equals(intent.getAction())) openCall();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent == null ? null : intent.getAction();
        if (PRESS.equals(action)) {
            int button = intent.getIntExtra("button", 0);
            try {
                muted = press(button);
            } catch (UnsatisfiedLinkError error) {
                // A late press on a notification from a process that has ended.
                Log.w("RustTalk", "Call button without the app", error);
            }
            // Hang up already stopped this service through MainActivity.communicationMode.
            if (button == HANG_UP) stopSelf();
            else if (foreground) post();
        } else if (SHOW.equals(action)) {
            name = intent.getStringExtra("name");
            server = intent.getStringExtra("server");
            muted = intent.getBooleanExtra("muted", false);
            canMute = intent.getBooleanExtra("canMute", true);
            if (foreground) post();
        } else {
            NotificationManager manager = getSystemService(NotificationManager.class);
            manager.createNotificationChannel(new NotificationChannel(CHANNEL,
                    getString(R.string.call_channel), NotificationManager.IMPORTANCE_LOW));
            foreground = post();
        }
        // A late button or update can start a service for a call that has ended.
        if (!foreground) stopSelf(startId);
        // A restarted process has no call to keep alive.
        return START_NOT_STICKY;
    }

    // Also updates the notification of a running service (same ID).
    private boolean post() {
        try {
            if (Build.VERSION.SDK_INT >= 29)
                startForeground(ID, build(), ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE);
            else
                startForeground(ID, build());
            return true;
        } catch (IllegalStateException | SecurityException error) {
            // ForegroundServiceStartNotAllowedException (Android 12+, an IllegalStateException)
            // when the app isn't in the foreground; SecurityException without microphone
            // access (Android 14+). The call goes on while the app stays on screen.
            Log.w("RustTalk", "Call foreground service not allowed", error);
            return false;
        }
    }

    private PendingIntent button(int button) {
        Intent intent = new Intent(this, CallService.class).setAction(PRESS).putExtra("button", button);
        return PendingIntent.getService(this, button, intent,
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    private Notification.Action action(int icon, int label, int button) {
        return new Notification.Action.Builder(Icon.createWithResource(this, icon),
                getString(label), button(button)).build();
    }

    private Notification build() {
        // Brings the running activity to the front (never a second one) and tells it to
        // show the call (MainActivity.onNewIntent).
        Intent open = new Intent(this, MainActivity.class).setAction(OPEN)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP
                        | Intent.FLAG_ACTIVITY_SINGLE_TOP);
        String title = getString(R.string.call_title);
        // The lock screen shows this when the phone hides private notification content:
        // no channel or person names.
        Notification hidden = new Notification.Builder(this, CHANNEL)
                .setSmallIcon(R.drawable.ic_call)
                .setContentTitle(title)
                .setContentText(getString(R.string.call_text))
                .build();
        Notification.Builder notification = new Notification.Builder(this, CHANNEL)
                .setSmallIcon(R.drawable.ic_call)
                .setCategory(Notification.CATEGORY_CALL)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setVisibility(Notification.VISIBILITY_PRIVATE)
                .setPublicVersion(hidden)
                .setContentIntent(PendingIntent.getActivity(this, 0, open,
                        PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT));
        if (server != null && !server.isEmpty()) notification.setSubText(server);
        if (canMute)
            notification.addAction(muted
                    ? action(R.drawable.ic_mic_off, R.string.call_unmute, MUTE)
                    : action(R.drawable.ic_call, R.string.call_mute, MUTE));
        boolean named = name != null && !name.isEmpty();
        if (Build.VERSION.SDK_INT >= 31) {
            notification.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE);
            // The system adds the Hang up button, a call chip in the status bar and
            // "Ongoing call".
            Person caller = new Person.Builder().setName(named ? name : title).setImportant(true).build();
            notification.setStyle(Notification.CallStyle.forOngoingCall(caller, button(HANG_UP)));
        } else {
            notification.setContentTitle(title)
                    .setContentText(named ? name : getString(R.string.call_text))
                    .addAction(action(R.drawable.ic_call, R.string.call_hang_up, HANG_UP));
        }
        return notification.build();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
