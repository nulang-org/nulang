interface ContactPayload {
  name?: string;
  email?: string;
  subject?: string;
  message?: string;
  turnstileToken?: string;
}

interface Env {
  TURNSTILE_SECRET: string;
  CLOUDFLARE_API_TOKEN: string;
  CLOUDFLARE_ACCOUNT_ID?: string;
  CONTACT_TO?: string;
}

const ACCOUNT_ID = 'ad5ebd496c88731aa7a6b4cfca58e612';
const MAX_NAME = 120;
const MAX_EMAIL = 254;
const MAX_SUBJECT = 200;
const MAX_MESSAGE = 5000;
const CONTACT_FROM = 'support@nulang.org';
const DEFAULT_CONTACT_TO = 'davidporkka@gmail.com';

function jsonResponse(body: Record<string, unknown>, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      'Content-Type': 'application/json; charset=utf-8',
      'Cache-Control': 'no-store',
    },
  });
}

function isValidEmail(value: string): boolean {
  return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(value);
}

function escapeHtml(value: string): string {
  return value
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

async function verifyTurnstile(
  secret: string,
  token: string,
  remoteIp: string | null,
): Promise<boolean> {
  const body = new URLSearchParams({
    secret,
    response: token,
  });
  if (remoteIp) {
    body.set('remoteip', remoteIp);
  }
  const response = await fetch('https://challenges.cloudflare.com/turnstile/v0/siteverify', {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body,
  });
  if (!response.ok) {
    return false;
  }
  const result = (await response.json()) as { success?: boolean };
  return result.success === true;
}

async function sendContactEmail(
  env: Env,
  destination: string,
  replyTo: string,
  subject: string,
  html: string,
  text: string,
): Promise<void> {
  const accountId = env.CLOUDFLARE_ACCOUNT_ID?.trim() || ACCOUNT_ID;
  const response = await fetch(
    `https://api.cloudflare.com/client/v4/accounts/${accountId}/email/sending/send`,
    {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${env.CLOUDFLARE_API_TOKEN}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({
        to: destination,
        from: { address: CONTACT_FROM, name: 'Nulang Contact Form' },
        reply_to: replyTo,
        subject,
        html,
        text,
      }),
    },
  );

  if (!response.ok) {
    const errorBody = await response.text();
    throw new Error(`email send failed (${response.status}): ${errorBody}`);
  }

  const result = (await response.json()) as { success?: boolean };
  if (!result.success) {
    throw new Error('email send failed: API returned success=false');
  }
}

export const onRequestPost: PagesFunction<Env> = async (context) => {
  let payload: ContactPayload;
  try {
    payload = (await context.request.json()) as ContactPayload;
  } catch {
    return jsonResponse({ ok: false, error: 'Invalid request body.' }, 400);
  }

  const name = payload.name?.trim() ?? '';
  const email = payload.email?.trim() ?? '';
  const subject = payload.subject?.trim() ?? '';
  const message = payload.message?.trim() ?? '';
  const turnstileToken = payload.turnstileToken?.trim() ?? '';

  if (!name || !email || !subject || !message || !turnstileToken) {
    return jsonResponse({ ok: false, error: 'All fields are required.' }, 400);
  }
  if (name.length > MAX_NAME || email.length > MAX_EMAIL || subject.length > MAX_SUBJECT) {
    return jsonResponse({ ok: false, error: 'One or more fields are too long.' }, 400);
  }
  if (message.length > MAX_MESSAGE) {
    return jsonResponse({ ok: false, error: 'Message is too long.' }, 400);
  }
  if (!isValidEmail(email)) {
    return jsonResponse({ ok: false, error: 'Please enter a valid email address.' }, 400);
  }

  const remoteIp =
    context.request.headers.get('CF-Connecting-IP') ??
    context.request.headers.get('X-Forwarded-For')?.split(',')[0]?.trim() ??
    null;

  const turnstileOk = await verifyTurnstile(context.env.TURNSTILE_SECRET, turnstileToken, remoteIp);
  if (!turnstileOk) {
    return jsonResponse({ ok: false, error: 'Bot verification failed. Please try again.' }, 403);
  }

  const destination = context.env.CONTACT_TO?.trim() || DEFAULT_CONTACT_TO;
  const safeName = escapeHtml(name);
  const safeEmail = escapeHtml(email);
  const safeSubject = escapeHtml(subject);
  const safeMessage = escapeHtml(message).replaceAll('\n', '<br />');
  const emailSubject = `[nulang.org] ${subject}`;

  try {
    await sendContactEmail(
      context.env,
      destination,
      email,
      emailSubject,
      `
        <h2>New contact form submission</h2>
        <p><strong>Name:</strong> ${safeName}</p>
        <p><strong>Email:</strong> ${safeEmail}</p>
        <p><strong>Subject:</strong> ${safeSubject}</p>
        <p><strong>Message:</strong></p>
        <p>${safeMessage}</p>
      `,
      `New contact form submission\n\nName: ${name}\nEmail: ${email}\nSubject: ${subject}\n\nMessage:\n${message}`,
    );
  } catch (error) {
    console.error('contact form email send failed', error);
    return jsonResponse({ ok: false, error: 'Unable to send message right now. Please email support@nulang.org directly.' }, 502);
  }

  return jsonResponse({ ok: true });
};
