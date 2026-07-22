// The curated one-click app catalog shown on the Apps page.
// `map_user` runs the container as the image's expected unprivileged UID.

export const CATALOG = [
  {
    id: "nginx", title: "Nginx", tag: "Web server", icon: "N", port: 8080, map_user: "101",
    image: "nginxinc/nginx-unprivileged:latest",
    desc: "A fast little web server. Start it and open it in your browser — it serves a welcome page you can replace with your own.",
  },
  {
    id: "redis", title: "Redis", tag: "Key-value database", icon: "R", port: 6379, map_user: "999",
    image: "redis:7-alpine",
    desc: "A lightning-fast in-memory database, used by countless apps for caching and queues.",
  },
  {
    id: "postgres", title: "PostgreSQL", tag: "SQL database", icon: "P", port: 5432, map_user: "999",
    image: "postgres:16", env: ["POSTGRES_PASSWORD=mycel"],
    desc: "The world's favourite SQL database. Password is preset to \u201cmycel\u201d.",
  },
  {
    id: "mongo", title: "MongoDB", tag: "Document database", icon: "M", port: 27017, map_user: "999",
    image: "mongo:7",
    desc: "A popular database that stores flexible JSON-like documents.",
  },
  {
    id: "python", title: "Python file server", tag: "Handy tool", icon: "Py", port: 8000,
    image: "python:3.12-alpine", command: ["python3", "-m", "http.server", "8000"],
    desc: "Shares files over the web using Python's built-in server — a nice way to see a container do something real.",
  },
  {
    id: "node", title: "Node.js hello app", tag: "Tiny web app", icon: "J", port: 3000,
    image: "node:22-alpine",
    command: ["node", "-e", "require('http').createServer((q,s)=>{s.setHeader('content-type','text/html');s.end('<h1>Hello from Node.js inside Mycel</h1>')}).listen(3000,()=>console.log('listening on 3000'))"],
    desc: "A one-line JavaScript web app. Start it and open it — instant proof your computer can run apps in containers.",
  },
];
