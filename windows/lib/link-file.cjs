'use strict';
// Native link file types: Windows Internet Shortcut (.url); freedesktop Link
// desktop entries (.desktop) on Linux, which KDE and GNOME file managers open.
function linkFile(url,{platform=process.platform}={}){
 if(typeof url!=='string'||/[\x00-\x1f\x7f]/.test(url))throw new Error('Invalid link.');
 let parsed;try{parsed=new URL(url);}catch{throw new Error('Invalid link.');}
 if(!['http:','https:'].includes(parsed.protocol)||!parsed.hostname)throw new Error('Invalid link.');
 if(platform==='linux'){
  // Desktop entry strings unescape \\, \n, \t, \r and \s, and a URL query may hold
  // a raw backslash, so each one is written escaped.
  const name=(parsed.hostname||'Link').replace(/[\x00-\x1f\x7f]/g,'')||'Link';
  return{extension:'.desktop',bytes:'[Desktop Entry]\nType=Link\nName='+name+'\nURL='+url.replaceAll('\\','\\\\')+'\nIcon=text-html\n'};
 }
 return{extension:'.url',bytes:'[InternetShortcut]\r\nURL='+url+'\r\n'};
}
module.exports={linkFile};
