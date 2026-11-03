# When chat programs join a person's consecutive messages

The question: when a person sends several messages in a row, under what rules do other chat programs show the later ones as plain lines under the first, without a new name, picture or space? The answer, read from each program's own source code, is that every one requires the same sender and the same day, and most add a time limit between the two messages, from 3 minutes in Signal Desktop to 15 in Telegram Desktop. Zulip alone has no time limit. Each rule was read from the files listed under Sources, at the commits linked there.

## Rules by program

| Program | Time limit between the two messages | Same sender | Same day | Other breaks |
|---|---|---|---|---|
| Signal Desktop | under 3 minutes (`COLLAPSE_WITHIN`) | yes | yes | the unread marker between them; the earlier message has reactions; the later is older than the earlier (an edit excepted). The earlier keeps its time and status when it is edited, still sending, failed, or the later was deleted and it was not |
| Element (Matrix) | at most 5 minutes (`CONTINUATION_MAX_INTERVAL`) | yes, and the sender's display name and picture unchanged between them | not checked in this rule | one deleted and the other not; different event types outside a short list; a thread summary on either |
| Telegram Desktop | under 900 seconds (`kAttachMessageToPreviousSecondsDelta`) | yes | a date badge breaks it | the unread bar; service messages; a message with an inline keyboard; different forum topics |
| Mattermost | at most 5 minutes (`POST_COLLAPSE_TIMEOUT`) | yes | not checked in this rule | webhook and system posts; a later post carrying AI-generated metadata, or the two marked differently; burn-on-read posts |
| Rocket.Chat | under the server's `Message_GroupingPeriod`, 300 seconds by default and set by the administrator | yes, and the same alias | yes | system messages; a thread reply joins only its thread; a message marked not groupable |
| Zulip | none | yes | yes | a different stream or topic; a status (`/me`) message on either side |
| Discord | not published | yes | | Its window is widely said to be several minutes; no official source states it, and Discord's code is not open |

## What the rules agree on

- The same sender, everywhere; the same calendar day in Signal Desktop, Telegram Desktop, Rocket.Chat and Zulip, the others' rules not looking at the day.
- A time limit in all but Zulip, of 3 to 15 minutes, 5 being the most common; Rocket.Chat lets an administrator choose it.
- Whatever stands between two messages breaks them apart: a date line, the unread marker, a system or service notice.
- Messages that need their own time or status shown keep it: Signal Desktop does not hide the time of a message still sending or failed, and of one edited.

## Rule gink follows

With "Compact on" (`compact 1` in the settings file), a message stands as a line under the one before, with no name, picture or time over it, when both are text from the same person, the same day, less than 5 minutes apart, with no unread marker between and no reactions under the first (`stacked` in `src/main.c`). Unlike Signal Desktop, a message still sending, edited or with a change waiting is joined too, since those states change after it is drawn and the line would jump; the state is written at the line's top right instead, where its time and menu show while the pointer is on it. The chat log layout keeps its columns either way.

## Sources

- Signal Desktop: [ts/util/timelineUtil.std.ts](https://github.com/signalapp/Signal-Desktop/blob/5c1a030485ea64a1c311288cd6b5eecbe36964cd/ts/util/timelineUtil.std.ts)
- Element: [apps/web/src/components/structures/MessagePanel.tsx](https://github.com/element-hq/element-web/blob/a9a05a82255398488e5d9780bc6ed4bb96511d08/apps/web/src/components/structures/MessagePanel.tsx)
- Telegram Desktop: [Telegram/SourceFiles/history/view/history_view_element.cpp](https://github.com/telegramdesktop/tdesktop/blob/80e6a03ac31fe7b14c43e80e60bedb3eb8d750d1/Telegram/SourceFiles/history/view/history_view_element.cpp)
- Mattermost: [webapp/channels/src/utils/post_utils.ts](https://github.com/mattermost/mattermost/blob/b4beed37f88b15d6eb50d4dff0be082043077b55/webapp/channels/src/utils/post_utils.ts) and [mattermost-redux/src/constants/posts.ts](https://github.com/mattermost/mattermost/blob/3a820143a1a5384172386a6c728e65825696a1e0/webapp/channels/src/packages/mattermost-redux/src/constants/posts.ts)
- Rocket.Chat: [apps/meteor/server/settings/message.ts](https://github.com/RocketChat/Rocket.Chat/blob/207613f819c018d802b7e86188f67812e9c44297/apps/meteor/server/settings/message.ts) and [MessageList/lib/isMessageSequential.ts](https://github.com/RocketChat/Rocket.Chat/blob/c887a1566970146864099a13f7f1b7d0f8525817/apps/meteor/client/views/room/MessageList/lib/isMessageSequential.ts)
- Zulip: [web/src/message_list_view.ts](https://github.com/zulip/zulip/blob/0436c196292cc02ceb73bb477a3f4dc11a05e9aa/web/src/message_list_view.ts)
