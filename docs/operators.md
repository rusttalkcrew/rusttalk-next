# Handling illegal content

This page is for the person who runs a RustTalk server: its owner, who hosts
it. It says what RustTalk lets you do when you learn that illegal content is
on your server, for example from a member's report, a notice from the
authorities or a rights holder. It is practical guidance, not legal advice.
What the law asks of you depends on where you and your members are; when in
doubt, ask a lawyer or the authority that contacted you.

## What your server can and can't see

Messages, files, names and pictures are end-to-end encrypted on members'
devices. Your server stores only ciphertext and can't read any of it, so it
can't scan for illegal content, and neither can you from the server's files.
You see content the same way members do: in the app, in channels you can
read. A notice usually names a message or file; the steps below work by that
reference. See [privacy.md](privacy.md) for exactly what the server stores.

## When you get a notice

1. **Find the content.** If you can see the message in the app, open its menu.
   Otherwise ask the reporter for the message ID: anyone who can read it can
   use "Copy message ID" in its menu.
2. **Remove it.** In the app, as the owner, use "Remove content" in the
   message's menu, or Server settings > Takedowns > Remove content and paste
   the message or file IDs. Write a short reason, and the notice's reference
   (case or ticket number) if it has one. The server then deletes:
   - the message and, from the menu, its edits;
   - every stored file and thumbnail the message carries, from disk;
   - the file from the channel's shared files, and from a page that kept it.

   It overwrites the deleted database rows and empties the database log, so
   the ciphertext doesn't linger on the server. Members' apps remove the
   message as soon as the server tells them.
3. **Ban the account if needed.** Tick "Also ban the account that posted it"
   in the same step, or ban them later from the member list. A ban signs them
   out and stops them coming back with the same identity.
4. **Keep the record.** The server keeps a record of every takedown under
   Server settings > Takedowns: what was removed (IDs and a SHA-256
   fingerprint of each encrypted item), when, by whom, your reason and the
   notice reference. It holds no content. Use "Copy record" to keep a copy
   with the notice. Your reason is stored in plain text, so describe the
   content ("image reported in notice 42"), don't copy it.
5. **Contact the authorities where required.** Some content, such as child
   sexual abuse material, must be reported to the police or a national
   hotline in many countries, and you may be asked to preserve information.
   Don't keep or forward the content yourself unless the authorities tell you
   how. The takedown record and the audit log (Server settings > Audit log)
   show what happened without the content.
6. **Reply to the notice** with what you did, if it asks you to.

## What a takedown can't reach

- **Copies on members' devices.** Apps remove the message when they hear it was
  deleted, but anyone could have saved or forwarded it before. A device that
  is offline removes it when it next connects.
- **Other servers.** A DM is stored on every server both people share; a
  takedown removes only your server's copy. Content posted on another server
  is up to that server's owner.
- **Your own backups.** RustTalk keeps no backups of its own. If you back up
  the `api-data` volume (see [operations.md](operations.md#ownership-and-backups)),
  older backups still hold the encrypted content. Delete or rotate them as
  your backup policy and the notice require.

## Good practice

- Keep the owner's devices safe: only the owner can remove content this way.
- Write down how members can reach you, for example in the server description.
- Act quickly once you know about illegal content, and keep your records.
