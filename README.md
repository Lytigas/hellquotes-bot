[Bot add url](https://discord.com/oauth2/authorize?client_id=1081104783471017995&scope=bot&permissions=3072)

# Data Flow

The discord bot receives commands and forwards to the web application. This ensures auth is taken care of for us.

Posting in the channel is handled entirely separately by watching the file
system for changes to the database.
